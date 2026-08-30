mod gateway;
mod pairing;

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use gateway::Gateway;
use pairing::Pairing;

const ACCESS_TTL: Duration = Duration::from_secs(30 * 60);
const CONTROL_TTL: Duration = Duration::from_secs(15 * 60);
const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AccessMode {
    ReadOnly,
    Control,
}

impl AccessMode {
    pub(super) fn ttl(self) -> Duration {
        match self {
            Self::ReadOnly => ACCESS_TTL,
            Self::Control => CONTROL_TTL,
        }
    }

    pub(super) fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::ReadOnly => &["read"],
            Self::Control => &["read", "workspace", "agent", "terminal"],
        }
    }
}

pub(super) struct AccessSession {
    mode: AccessMode,
    gateway: Gateway,
    delegated: DelegatedToken,
    pairing_code: String,
    pairing_expires_at: u64,
}

impl AccessSession {
    pub(super) fn start(mode: AccessMode, context: crate::i18n::cli::Context) -> Result<Self> {
        probe_server(context)?;
        let delegated = DelegatedToken::create(mode)
            .map_err(|_| anyhow!(context.text("Could not authorize UHP access.")))?;
        let pairing = Pairing::new(PAIRING_TTL)
            .map_err(|_| anyhow!(context.text("Could not create a secure pairing code.")))?;
        let pairing_code = pairing.display_code().to_string();
        let pairing_expires_at = unix_now()?.saturating_add(PAIRING_TTL.as_secs());
        let gateway = Gateway::start(
            crate::persist::cli_socket_path(),
            delegated.secret.clone(),
            delegated.expires_at,
            pairing,
            mode,
        )
        .map_err(|_| anyhow!(context.text("Could not start the private UHP access gateway.")))?;
        Ok(Self {
            mode,
            gateway,
            delegated,
            pairing_code,
            pairing_expires_at,
        })
    }

    pub(super) fn port(&self) -> u16 {
        self.gateway.address().port()
    }

    fn descriptor(&self) -> Value {
        access_descriptor(
            self.mode,
            self.port(),
            &self.pairing_code,
            self.pairing_expires_at,
            self.delegated.expires_at,
        )
    }

    pub(super) fn stop(&mut self) {
        self.gateway.stop();
    }
}

/// Start the transport-neutral, authenticated UHP access endpoint. The one
/// stdout line is deliberately machine-readable so an independent transport
/// provider can launch this command, forward the loopback endpoint, and pass
/// the descriptor to any compatible client without knowing Luvus internals.
pub(crate) fn run_cli(args: &[String], context: crate::i18n::cli::Context) -> Result<i32> {
    let mode = parse_mode(args, "usage: luvus uhp access [--control]", context)?;
    let mut access = AccessSession::start(mode, context)?;
    shutdown::install();
    println!("{}", serde_json::to_string(&access.descriptor())?);
    std::io::stdout().flush()?;

    let deadline = std::time::Instant::now() + mode.ttl();
    while !shutdown::requested() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    access.stop();
    Ok(0)
}

pub(super) fn parse_mode(
    args: &[String],
    usage: &'static str,
    context: crate::i18n::cli::Context,
) -> Result<AccessMode> {
    match args {
        [] => Ok(AccessMode::ReadOnly),
        [flag] if flag == "--control" => Ok(AccessMode::Control),
        _ => Err(anyhow!(crate::i18n::cli::help(usage, context.language()))),
    }
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

fn access_descriptor(
    mode: AccessMode,
    port: u16,
    pairing_code: &str,
    pairing_expires_at: u64,
    authority_expires_at: u64,
) -> Value {
    json!({
        "$schema":"https://luvus.dev/protocol/uhp/v1/schema/access/descriptor.schema.json",
        "type":"luvus_uhp_access",
        "protocol":{
            "name":crate::api::PROTOCOL_NAME,
            "major":crate::api::PROTOCOL_MAJOR,
            "minor":crate::api::PROTOCOL_MINOR,
        },
        "access":{"major":1},
        "endpoint":{
            "transport":"tcp",
            "host":"127.0.0.1",
            "port":port,
            "framing":"ndjson",
        },
        "pairing":{
            "type":"one_use_code",
            "code":pairing_code,
            "expires_at":pairing_expires_at,
        },
        "authority":{
            "mode":match mode {
                AccessMode::ReadOnly => "read_only",
                AccessMode::Control => "control",
            },
            "scopes":mode.scopes(),
            "expires_at":authority_expires_at,
        },
    })
}

struct DelegatedToken {
    id: String,
    secret: String,
    expires_at: u64,
}

impl DelegatedToken {
    fn create(mode: AccessMode) -> Result<Self> {
        let response = local_request(
            "uhp.token.create",
            json!({"scopes":mode.scopes(),"ttl_s":mode.ttl().as_secs()}),
        )?;
        let result = response
            .get("result")
            .ok_or_else(|| response_error("cannot create delegated UHP access", &response))?;
        let id = result
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("invalid delegated-token response"))?;
        let secret = result
            .get("token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("invalid delegated-token response"))?;
        let expires_at = result
            .get("expires_at")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("invalid delegated-token response"))?;
        Ok(Self {
            id: id.to_string(),
            secret: secret.to_string(),
            expires_at,
        })
    }
}

impl Drop for DelegatedToken {
    fn drop(&mut self) {
        let _ = local_request("uhp.token.revoke", json!({"id":self.id}));
        self.secret.clear();
    }
}

fn probe_server(context: crate::i18n::cli::Context) -> Result<()> {
    let response = local_request("ping", Value::Null).map_err(|_| {
        anyhow!(
            "{} (socket: {})",
            context.text("no luvus server running"),
            crate::persist::cli_socket_path().display()
        )
    })?;
    if response.get("error").is_some() {
        return Err(response_error(
            "selected Luvus server did not answer",
            &response,
        ));
    }
    Ok(())
}

fn local_request(method: &str, params: Value) -> Result<Value> {
    let request_id = crate::terminal::backend::random_id().map_err(anyhow::Error::msg)?;
    let path = crate::persist::cli_socket_path();
    let mut stream = crate::ipc::transport::connect(&path).with_context(|| {
        format!(
            "cannot connect to selected Luvus server ({})",
            path.display()
        )
    })?;
    writeln!(
        stream,
        "{}",
        json!({"id":request_id,"method":method,"params":params})
    )?;
    stream.flush()?;
    let response =
        crate::ipc::api::read_response_frame_with_deadline(&mut stream, Duration::from_secs(5))?;
    let value: Value = serde_json::from_str(&response).context("invalid local UHP response")?;
    if value.get("id").and_then(Value::as_str) != Some(request_id.as_str()) {
        return Err(anyhow!("local UHP response id mismatch"));
    }
    Ok(value)
}

fn response_error(context: &'static str, response: &Value) -> anyhow::Error {
    let code = response
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    anyhow!("{context} ({code})")
}

#[cfg(unix)]
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    pub(super) fn requested() -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }

    pub(super) fn install() {
        extern "C" fn on_signal(_signal: libc::c_int) {
            REQUESTED.store(true, Ordering::Relaxed);
        }
        unsafe {
            let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGTERM, handler);
            libc::signal(libc::SIGHUP, handler);
        }
    }
}

#[cfg(windows)]
mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{BOOL, TRUE};
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn on_control(_kind: u32) -> BOOL {
        REQUESTED.store(true, Ordering::Relaxed);
        TRUE
    }

    pub(super) fn requested() -> bool {
        REQUESTED.load(Ordering::Relaxed)
    }

    pub(super) fn install() {
        unsafe {
            SetConsoleCtrlHandler(Some(on_control), TRUE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiries_are_bounded() {
        assert!(ACCESS_TTL <= Duration::from_secs(86_400));
        assert!(CONTROL_TTL < ACCESS_TTL);
        assert_eq!(PAIRING_TTL, Duration::from_secs(300));
    }

    #[test]
    fn control_authority_is_explicit_and_bounded() {
        assert_eq!(AccessMode::ReadOnly.scopes(), &["read"]);
        assert_eq!(
            AccessMode::Control.scopes(),
            &["read", "workspace", "agent", "terminal"]
        );
    }

    #[test]
    fn descriptor_is_transport_neutral_and_does_not_disclose_token() {
        let descriptor = access_descriptor(
            AccessMode::Control,
            43123,
            "ABCD-EFGH-JKLM",
            1_700_000_300,
            1_700_000_900,
        );
        assert_eq!(descriptor["type"], "luvus_uhp_access");
        assert_eq!(descriptor["protocol"]["major"], 1);
        assert_eq!(descriptor["endpoint"]["host"], "127.0.0.1");
        assert_eq!(descriptor["endpoint"]["transport"], "tcp");
        assert_eq!(descriptor["endpoint"]["framing"], "ndjson");
        assert_eq!(descriptor["authority"]["mode"], "control");
        assert_eq!(descriptor["authority"]["scopes"][3], "terminal");
        assert!(descriptor.get("token").is_none());
        assert!(descriptor["authority"].get("token").is_none());
    }
}
