mod assets;
mod auth;
mod server;
mod uhp;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use auth::BrowserAuthority;
use server::{normalize_origin, BridgeState};
use uhp::UhpAccess;

const USAGE: &str = "\
Usage: luvus [--session <name>] web [options]

Serve the optional browser client for the selected Luvus session. The web
bridge runs in the foreground and stops without stopping the Luvus server,
its PTYs, or attached TUI clients.

Options:
  --control             allow bounded terminal and workspace control
  --read-only           explicitly select the default read-only authority
  --port <port>         loopback port (default: 4174; 0 selects a free port)
  --max-devices <1-8>   authorized browser devices (default: 2)
  --public-url <origin> public HTTP(S) origin used in pairing links
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
    let state = BridgeState::new(
        authority,
        Arc::clone(&uhp),
        options.origins,
        options.public_url.clone(),
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        options.port,
    ))
    .await
    .with_context(|| format!("could not bind Luvus Web to 127.0.0.1:{}", options.port))?;
    let port = listener.local_addr()?.port();
    let base = options
        .public_url
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
    let url = format!("{base}/#pair={}", initial.code);
    println!("Luvus Web is ready for session '{session}'.");
    println!("{url}");
    println!("Press Ctrl+C to stop the web bridge. Luvus and its panes will keep running.");
    if !options.no_open {
        open_browser(&url);
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
                        normalize_origin(raw)
                            .ok_or("--public-url must be an HTTP(S) origin without a path")?,
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

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, arguments): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let (program, arguments): (&str, Vec<&str>) = ("cmd.exe", vec!["/c", "start", "", url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (program, arguments): (&str, Vec<&str>) = ("xdg-open", vec![url]);

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
        assert!(Options::parse(&strings(&["--origin", "https://phone.example/path"])).is_err());
    }
}
