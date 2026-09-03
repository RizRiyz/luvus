mod gateway;
mod pairing;

use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use gateway::Gateway;
use pairing::Pairing;

const DEFAULT_ACCESS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);
const TOKEN_REFRESH_WINDOW: u64 = 5 * 60;
const MAX_ACCESS_TTL_SECS: u64 = 24 * 60 * 60;
const ACCESS_USAGE: &str = "usage: luvus uhp access [--control] [--ttl <seconds> | --no-expiry]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AccessMode {
    ReadOnly,
    Control,
}

impl AccessMode {
    pub(super) fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::ReadOnly => &["read"],
            Self::Control => &["read", "workspace", "agent", "terminal", "orchestration"],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AccessOptions {
    mode: AccessMode,
    lifetime: AccessLifetime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccessLifetime {
    Finite(Duration),
    Process,
}

struct AccessSession {
    mode: AccessMode,
    gateway: Gateway,
    delegated: DelegatedToken,
    retired: Vec<DelegatedToken>,
    pairing_code: String,
    pairing_expires_at: u64,
    authority_expires_at: Option<u64>,
}

impl AccessSession {
    fn start(
        mode: AccessMode,
        lifetime: AccessLifetime,
        context: crate::i18n::cli::Context,
    ) -> Result<Self> {
        probe_server(context)?;
        let token_ttl = match lifetime {
            AccessLifetime::Finite(ttl) => ttl,
            AccessLifetime::Process => DEFAULT_ACCESS_TTL,
        };
        let delegated = DelegatedToken::create(mode, token_ttl)
            .map_err(|_| anyhow!(context.text("Could not authorize UHP access.")))?;
        let authority_expires_at = match lifetime {
            AccessLifetime::Finite(_) => Some(delegated.expires_at),
            AccessLifetime::Process => None,
        };
        let pairing_ttl = PAIRING_TTL.min(token_ttl);
        let pairing = Pairing::new(pairing_ttl)
            .map_err(|_| anyhow!(context.text("Could not create a secure pairing code.")))?;
        let pairing_code = pairing.display_code().to_string();
        let pairing_expires_at = unix_now()?.saturating_add(pairing_ttl.as_secs());
        let client_token = format!(
            "luv_access_{}",
            crate::terminal::backend::random_id().map_err(anyhow::Error::msg)?
        );
        let gateway = Gateway::start(
            crate::persist::cli_socket_path(),
            client_token,
            authority_expires_at,
            delegated.secret.clone(),
            pairing,
            mode,
        )
        .map_err(|_| anyhow!(context.text("Could not start the private UHP access gateway.")))?;
        Ok(Self {
            mode,
            gateway,
            delegated,
            retired: Vec::new(),
            pairing_code,
            pairing_expires_at,
            authority_expires_at,
        })
    }

    fn port(&self) -> u16 {
        self.gateway.address().port()
    }

    fn descriptor(&self) -> Value {
        access_descriptor(
            self.mode,
            self.port(),
            &self.pairing_code,
            self.pairing_expires_at,
            self.authority_expires_at,
        )
    }

    fn refresh_process_authority(&mut self, context: crate::i18n::cli::Context) -> Result<()> {
        let now = unix_now()?;
        self.retired.retain(|token| token.expires_at > now);
        if self.delegated.expires_at.saturating_sub(now) > TOKEN_REFRESH_WINDOW {
            return Ok(());
        }
        let next = DelegatedToken::create(self.mode, DEFAULT_ACCESS_TTL)
            .map_err(|_| anyhow!(context.text("Could not authorize UHP access.")))?;
        self.gateway.replace_upstream_token(next.secret.clone());
        self.retired
            .push(std::mem::replace(&mut self.delegated, next));
        Ok(())
    }

    fn stop(&mut self) {
        self.gateway.cancel();
        self.delegated.revoke();
        for token in &mut self.retired {
            token.revoke();
        }
        self.gateway.finish_stop();
    }
}

impl Drop for AccessSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the transport-neutral, authenticated UHP access endpoint. The one
/// stdout line is deliberately machine-readable so an independent transport
/// provider can launch this command, forward the loopback endpoint, and pass
/// the descriptor to any compatible client without knowing Luvus internals.
pub(crate) fn run_cli(args: &[String], context: crate::i18n::cli::Context) -> Result<i32> {
    let options = parse_options(args, context)?;
    let mut access = AccessSession::start(options.mode, options.lifetime, context)?;
    shutdown::install();
    println!("{}", serde_json::to_string(&access.descriptor())?);
    std::io::stdout().flush()?;

    let deadline = match options.lifetime {
        AccessLifetime::Finite(ttl) => Some(Instant::now() + ttl),
        AccessLifetime::Process => None,
    };
    while !shutdown::requested() && deadline.is_none_or(|deadline| Instant::now() < deadline) {
        if options.lifetime == AccessLifetime::Process {
            access.refresh_process_authority(context)?;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    access.stop();
    Ok(0)
}

fn parse_options(args: &[String], context: crate::i18n::cli::Context) -> Result<AccessOptions> {
    let mut mode = AccessMode::ReadOnly;
    let mut lifetime = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--control" if mode == AccessMode::ReadOnly => {
                mode = AccessMode::Control;
                index += 1;
            }
            "--ttl" if lifetime.is_none() => {
                let seconds = args
                    .get(index + 1)
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|seconds| (1..=MAX_ACCESS_TTL_SECS).contains(seconds))
                    .ok_or_else(|| {
                        anyhow!(crate::i18n::cli::help(ACCESS_USAGE, context.language()))
                    })?;
                lifetime = Some(AccessLifetime::Finite(Duration::from_secs(seconds)));
                index += 2;
            }
            "--no-expiry" if lifetime.is_none() => {
                lifetime = Some(AccessLifetime::Process);
                index += 1;
            }
            _ => {
                return Err(anyhow!(crate::i18n::cli::help(
                    ACCESS_USAGE,
                    context.language()
                )))
            }
        }
    }
    Ok(AccessOptions {
        mode,
        lifetime: lifetime.unwrap_or(AccessLifetime::Finite(DEFAULT_ACCESS_TTL)),
    })
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
    authority_expires_at: Option<u64>,
) -> Value {
    let mut descriptor = json!({
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
        },
    });
    if let Some(expires_at) = authority_expires_at {
        descriptor["authority"]["expires_at"] = json!(expires_at);
    } else {
        descriptor["authority"]["expires_on_close"] = json!(true);
    }
    descriptor
}

struct DelegatedToken {
    id: String,
    secret: String,
    expires_at: u64,
    revoked: bool,
}

impl DelegatedToken {
    fn create(mode: AccessMode, ttl: Duration) -> Result<Self> {
        let response = local_request(
            "uhp.token.create",
            json!({"scopes":mode.scopes(),"ttl_s":ttl.as_secs()}),
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
            revoked: false,
        })
    }

    fn revoke(&mut self) {
        if self.revoked {
            return;
        }
        self.revoked = true;
        let _ = local_request("uhp.token.revoke", json!({"id":self.id}));
        self.secret.clear();
    }
}

impl Drop for DelegatedToken {
    fn drop(&mut self) {
        self.revoke();
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

    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::TRUE;
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
        assert_eq!(DEFAULT_ACCESS_TTL, Duration::from_secs(86_400));
        assert_eq!(PAIRING_TTL, Duration::from_secs(300));
        assert_eq!(MAX_ACCESS_TTL_SECS, 86_400);
    }

    #[test]
    fn access_options_keep_safe_defaults_and_accept_a_bounded_ttl() {
        let context = crate::i18n::cli::Context::for_language(crate::i18n::cli::Language::En);
        assert_eq!(
            parse_options(&[], context).unwrap(),
            AccessOptions {
                mode: AccessMode::ReadOnly,
                lifetime: AccessLifetime::Finite(DEFAULT_ACCESS_TTL),
            }
        );
        assert_eq!(
            parse_options(&["--control".into()], context).unwrap(),
            AccessOptions {
                mode: AccessMode::Control,
                lifetime: AccessLifetime::Finite(DEFAULT_ACCESS_TTL),
            }
        );
        assert_eq!(
            parse_options(
                &["--ttl".into(), "7200".into(), "--control".into()],
                context
            )
            .unwrap(),
            AccessOptions {
                mode: AccessMode::Control,
                lifetime: AccessLifetime::Finite(Duration::from_secs(7200)),
            }
        );
        assert_eq!(
            parse_options(&["--no-expiry".into()], context).unwrap(),
            AccessOptions {
                mode: AccessMode::ReadOnly,
                lifetime: AccessLifetime::Process,
            }
        );
    }

    #[test]
    fn access_options_reject_invalid_or_ambiguous_ttls() {
        let context = crate::i18n::cli::Context::for_language(crate::i18n::cli::Language::En);
        for args in [
            vec!["--ttl".into()],
            vec!["--ttl".into(), "0".into()],
            vec!["--ttl".into(), "86401".into()],
            vec!["--ttl".into(), "60".into(), "--ttl".into(), "90".into()],
            vec!["--ttl".into(), "60".into(), "--no-expiry".into()],
            vec!["--no-expiry".into(), "--ttl".into(), "60".into()],
            vec!["--control".into(), "--control".into()],
        ] {
            assert!(parse_options(&args, context).is_err(), "{args:?}");
        }
    }

    #[test]
    fn control_authority_is_explicit_and_bounded() {
        assert_eq!(AccessMode::ReadOnly.scopes(), &["read"]);
        assert_eq!(
            AccessMode::Control.scopes(),
            &["read", "workspace", "agent", "terminal", "orchestration"]
        );
    }

    #[test]
    fn descriptor_is_transport_neutral_and_does_not_disclose_token() {
        let descriptor = access_descriptor(
            AccessMode::Control,
            43123,
            "ABCD-EFGH-JKLM",
            1_700_000_300,
            Some(1_700_000_900),
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

        let process_bound = access_descriptor(
            AccessMode::ReadOnly,
            43123,
            "ABCD-EFGH-JKLM",
            1_700_000_300,
            None,
        );
        assert_eq!(process_bound["authority"]["expires_on_close"], true);
        assert!(process_bound["authority"].get("expires_at").is_none());
    }
}
