mod assets;
mod auth;
mod server;
mod uhp;

use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use auth::BrowserAuthority;
use server::{allowed_hosts, normalize_origin, normalize_public_origin, BridgeState};
use uhp::UhpAccess;

/// How long the private launcher file for the auto-opened link is kept. The
/// browser reads it immediately; the file only needs to outlive that read.
const LAUNCHER_LIFETIME: Duration = Duration::from_secs(60);

const USAGE: &str = "\
Usage: luvus [--session <name>] web [options]

Serve the optional browser client for the selected Luvus session. The web
bridge runs in the foreground and stops without stopping the Luvus server,
its PTYs, or attached TUI clients.

Each pairing link works once, in one browser tab, and expires after five
minutes. Press Enter in this terminal while the bridge runs to print a new one.

Options:
  --control             allow bounded terminal and workspace control
  --read-only           explicitly select the default read-only authority
  --port <port>         loopback port (default: 4174; 0 selects a free port)
  --max-devices <1-8>   authorized browser devices (default: 2)
  --public-url <origin> public HTTPS origin used in pairing links
  --origin <origin>     allow a public HTTP(S) WebSocket origin (repeatable)
  --no-open             print the pairing URL without opening a browser
  --help, -h            show this help
";

pub(crate) fn run_cli(args: &[String]) -> Result<i32> {
    let options = match Options::parse(args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return Ok(0);
        }
        Err(message) => return Err(anyhow!("{message}\n\n{USAGE}")),
    };

    let selected = crate::session::active_name();
    crate::session::start_session(selected.as_deref())
        .map_err(anyhow::Error::msg)
        .context("could not start or attach the selected Luvus session")?;
    let session = crate::session::display_name();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("luvus-web")
        .build()
        .context("could not initialize the web runtime")?;
    runtime.block_on(run(session, options))?;
    Ok(0)
}

async fn run(session: String, options: Options) -> Result<()> {
    let uhp = Arc::new(
        UhpAccess::start(session.clone(), options.control)
            .map_err(anyhow::Error::msg)
            .context("could not establish scoped web authority")?,
    );
    let (authority, initial) =
        BrowserAuthority::new(12 * 60 * 60, options.max_devices).map_err(anyhow::Error::msg)?;
    // Bind before building the state: the Host allowlist needs the real port,
    // which `--port 0` only learns here.
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        options.port,
    ))
    .await
    .with_context(|| format!("could not bind Luvus Web to 127.0.0.1:{}", options.port))?;
    let port = listener.local_addr()?.port();
    let hosts = allowed_hosts(port, options.public_url.as_deref(), &options.origins);
    let state = BridgeState::new(
        authority,
        Arc::clone(&uhp),
        options.origins,
        hosts,
        options.public_url.clone(),
    );
    // Links printed here always use the address chosen at startup. A paired
    // browser can change the pairing address for the links it creates, but
    // never where the operator's own links point.
    let base = options
        .public_url
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
    let url = format!("{base}/#pair={}", initial.code);
    println!("Luvus Web is ready for session '{session}'.");
    println!("{url}");
    println!(
        "Open this link once, in one browser tab. It works one time and expires in 5 minutes."
    );
    let interactive = std::io::stdin().is_terminal();
    if interactive {
        println!("Press Enter here for a new one-use link.");
    }
    println!("Press Ctrl+C to stop the web bridge. Luvus and its panes will keep running.");
    let _launcher = if options.no_open {
        None
    } else {
        let launcher = open_browser_privately(&url);
        if launcher.is_none() {
            println!("Could not open a browser automatically; open the link above.");
        }
        launcher
    };
    if interactive {
        tokio::spawn(operator_links(state.clone(), base));
    }

    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("Luvus Web server failed")?;
    drop(uhp);
    Ok(())
}

/// Print a fresh one-use pairing link each time the operator presses Enter in
/// the terminal running the bridge. Only whoever controls that terminal can
/// do this, which is the same trust as reading the initial link.
async fn operator_links(state: BridgeState, base: String) {
    let (lines, mut entered) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Blocking stdin read on its own thread; it ends with the process.
    std::thread::spawn(move || {
        let mut line = String::new();
        while matches!(std::io::stdin().read_line(&mut line), Ok(read) if read > 0) {
            line.clear();
            if lines.send(()).is_err() {
                break;
            }
        }
    });
    while entered.recv().await.is_some() {
        let (pairing, revoked) = state.operator_pairing();
        println!(
            "{}",
            operator_link_message(&base, pairing.as_ref(), revoked)
        );
    }
}

fn operator_link_message(
    base: &str,
    pairing: Option<&auth::BrowserPairing>,
    revoked: usize,
) -> String {
    let Some(pairing) = pairing else {
        return "No room for another device: every allowed device is still connected. \
                Close a Luvus Web tab, or restart with a larger --max-devices."
            .to_string();
    };
    let mut message = String::new();
    if revoked > 0 {
        let noun = if revoked == 1 { "device" } else { "devices" };
        message.push_str(&format!(
            "Revoked {revoked} disconnected {noun} to make room.\n"
        ));
    }
    message.push_str(&format!(
        "New one-use link (expires in 5 minutes):\n{base}/#pair={}",
        pairing.code
    ));
    message
}

/// Removes the private launcher directory when dropped.
struct Launcher(PathBuf);

impl Drop for Launcher {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Open the pairing link without putting its secret on a command line.
///
/// Passing the URL to `open`/`xdg-open` would expose the one-use code in the
/// process list, where another local user could read it and redeem it first.
/// Instead the link goes into an owner-only HTML file that redirects to it, and
/// only that file's path is passed to the opener. If the file cannot be
/// written privately, nothing is opened; the terminal still shows the link.
fn open_browser_privately(url: &str) -> Option<Launcher> {
    let launcher = write_launcher(&std::env::temp_dir(), url).ok()?;
    let file = launcher.0.join("open.html");
    open_path(&file);
    let dir = launcher.0.clone();
    tokio::spawn(async move {
        tokio::time::sleep(LAUNCHER_LIFETIME).await;
        let _ = std::fs::remove_dir_all(dir);
    });
    Some(launcher)
}

/// Write `open.html` into a new owner-only directory under `parent`.
fn write_launcher(parent: &Path, url: &str) -> std::io::Result<Launcher> {
    use std::io::Write;
    let mut suffix = [0_u8; 12];
    getrandom::fill(&mut suffix).map_err(|error| std::io::Error::other(error.to_string()))?;
    let name: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let dir = parent.join(format!("luvus-web-{name}"));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    // `create`, not `create_all`: the directory must be new, so a name that an
    // attacker pre-created or linked elsewhere fails instead of being reused.
    builder.create(&dir)?;
    let launcher = Launcher(dir);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(launcher.0.join("open.html"))?;
    file.write_all(launcher_html(url).as_bytes())?;
    Ok(launcher)
}

fn launcher_html(url: &str) -> String {
    // JSON escaping alone would let `</script>` close the element, so `<` is
    // escaped too. The URL is a validated origin plus a base64url code today;
    // this keeps the launcher safe if either ever widens.
    let script = serde_json::to_string(url)
        .unwrap_or_else(|_| "\"about:blank\"".to_string())
        .replace('<', "\\u003c");
    let attribute = url
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<meta name=\"referrer\" content=\"no-referrer\">\n\
         <title>Opening Luvus Web</title>\n\
         <meta http-equiv=\"refresh\" content=\"0;url={attribute}\">\n\
         <script>location.replace({script});</script>\n"
    )
}

struct Options {
    control: bool,
    port: u16,
    max_devices: usize,
    public_url: Option<String>,
    origins: Vec<String>,
    no_open: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Option<Self>, String> {
        let mut control = false;
        let mut read_only = false;
        let mut port = 4174;
        let mut max_devices = 2;
        let mut public_url = None;
        let mut origins = Vec::new();
        let mut no_open = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--help" | "-h" => return Ok(None),
                "--control" => control = true,
                "--read-only" => read_only = true,
                "--no-open" => no_open = true,
                "--port" => {
                    let raw = args.get(index + 1).ok_or("--port requires a value")?;
                    port = raw
                        .parse::<u16>()
                        .map_err(|_| "--port must be an integer from 0 through 65535")?;
                    index += 1;
                }
                "--max-devices" => {
                    let raw = args
                        .get(index + 1)
                        .ok_or("--max-devices requires a value")?;
                    max_devices = raw
                        .parse::<usize>()
                        .ok()
                        .filter(|value| (1..=8).contains(value))
                        .ok_or("--max-devices must be an integer from 1 through 8")?;
                    index += 1;
                }
                "--public-url" => {
                    let raw = args.get(index + 1).ok_or("--public-url requires a value")?;
                    public_url = Some(
                        normalize_public_origin(raw)
                            .ok_or("--public-url must be an HTTPS origin without a path")?,
                    );
                    index += 1;
                }
                "--origin" => {
                    let raw = args.get(index + 1).ok_or("--origin requires a value")?;
                    origins.push(
                        normalize_origin(raw)
                            .ok_or("--origin must be an HTTP(S) origin without a path")?,
                    );
                    index += 1;
                }
                option => return Err(format!("unknown web option: {option}")),
            }
            index += 1;
        }
        if control && read_only {
            return Err("--control and --read-only cannot be used together".to_string());
        }
        origins.sort();
        origins.dedup();
        Ok(Some(Self {
            control,
            port,
            max_devices,
            public_url,
            origins,
            no_open,
        }))
    }
}

/// Open a local file with the platform's default handler. Only the file's path
/// reaches the command line; see [`open_browser_privately`].
fn open_path(path: &Path) {
    let path = path.as_os_str();
    #[cfg(target_os = "macos")]
    let (program, arguments): (&str, Vec<&std::ffi::OsStr>) = ("open", vec![path]);
    #[cfg(target_os = "windows")]
    let (program, arguments): (&str, Vec<&std::ffi::OsStr>) = (
        "cmd.exe",
        vec!["/c".as_ref(), "start".as_ref(), "".as_ref(), path],
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    let (program, arguments): (&str, Vec<&std::ffi::OsStr>) = ("xdg-open", vec![path]);

    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut command);
    let _ = command.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// The auto-open link travels in an owner-only file, so the pairing code
    /// never appears in the process list. The file is removed with its guard.
    #[test]
    fn the_launcher_is_private_and_removed_afterwards() {
        let parent = std::env::temp_dir();
        let url = "http://127.0.0.1:4174/#pair=abc-DEF_123";
        let launcher = write_launcher(&parent, url).unwrap();
        let dir = launcher.0.clone();
        let file = dir.join("open.html");
        let html = std::fs::read_to_string(&file).unwrap();
        assert!(html.contains("location.replace(\"http://127.0.0.1:4174/#pair=abc-DEF_123\")"));
        assert!(html.contains("content=\"0;url=http://127.0.0.1:4174/#pair=abc-DEF_123\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "only the owner may list or enter it");
            assert_eq!(file_mode, 0o600, "only the owner may read the code");
        }
        drop(launcher);
        assert!(!dir.exists(), "the launcher is deleted with its guard");
    }

    #[test]
    fn the_launcher_cannot_be_broken_out_of() {
        let html = launcher_html("http://x/#a</script><script>alert(1)</script>\"&");
        assert!(!html.contains("</script><script>"), "{html}");
        assert!(html.contains("\\u003c/script>"), "{html}");
        assert!(html.contains("&lt;/script&gt;") && html.contains("&quot;&amp;"));
    }

    #[test]
    fn operator_messages_say_what_happened() {
        let pairing = auth::BrowserPairing {
            code: "CODE".into(),
            expires_at: 0,
        };
        let fresh = operator_link_message("http://127.0.0.1:4174", Some(&pairing), 0);
        assert_eq!(
            fresh,
            "New one-use link (expires in 5 minutes):\nhttp://127.0.0.1:4174/#pair=CODE"
        );
        let reclaimed = operator_link_message("http://127.0.0.1:4174", Some(&pairing), 2);
        assert!(reclaimed.starts_with("Revoked 2 disconnected devices to make room.\n"));
        let full = operator_link_message("http://127.0.0.1:4174", None, 0);
        assert!(full.contains("every allowed device is still connected"));
    }

    #[test]
    fn options_are_read_only_and_loopback_by_default() {
        let options = Options::parse(&[]).unwrap().unwrap();
        assert!(!options.control);
        assert_eq!(options.port, 4174);
        assert_eq!(options.max_devices, 2);
        assert!(options.origins.is_empty());
    }

    #[test]
    fn options_validate_authority_devices_and_origins() {
        let options = Options::parse(&strings(&[
            "--control",
            "--port",
            "0",
            "--max-devices",
            "4",
            "--public-url",
            "https://phone.example/",
            "--origin",
            "https://phone.example",
            "--no-open",
        ]))
        .unwrap()
        .unwrap();
        assert!(options.control);
        assert_eq!(options.port, 0);
        assert_eq!(options.max_devices, 4);
        assert_eq!(options.public_url.as_deref(), Some("https://phone.example"));
        assert!(options.no_open);
        assert!(Options::parse(&strings(&["--control", "--read-only"])).is_err());
        assert!(Options::parse(&strings(&["--max-devices", "9"])).is_err());
        assert!(Options::parse(&strings(&["--public-url", "http://phone.example"])).is_err());
        assert!(Options::parse(&strings(&["--origin", "https://phone.example/path"])).is_err());
    }
}
