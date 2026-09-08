//! Explicit foreground provisioning for saved SSH machines.
//!
//! This path runs only while a user-authorized `machine add` or `machine
//! enable` request is in progress. Persistent links and reconnects never call
//! it. The remote script is embedded and fixed: profile data is passed only as
//! a validated OpenSSH destination, while the release version comes from this
//! build.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::catalog::{validate_destination, validate_remote_binary};

const PROVISION_TIMEOUT: Duration = Duration::from_secs(180);

// Download the exact release matching the controller, verify its published
// SHA-256 digest, prove the fleet capability before replacement, and install
// atomically into the remote account's private user prefix. No fetched script
// is executed and no administrator access is requested.
const INSTALL_POSIX: &str = r#"set -eu
umask 077
fail() { printf 'error: %s\n' "$1" >&2; exit 1; }
version=${1:-}
case "$version" in ''|*[!0-9A-Za-z.+-]*) fail 'invalid release version' ;; esac
case "$(uname -s):$(uname -m)" in
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
  Linux:x86_64) target=x86_64-unknown-linux-musl ;;
  Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
  *) fail 'unsupported remote operating system or architecture' ;;
esac
tag=v$version
stem=luvus-$tag-$target
archive=$stem.tar.gz
base=https://github.com/RizRiyz/luvus/releases/download/$tag
tmp=$(mktemp -d "${TMPDIR:-/tmp}/luvus-machine.XXXXXX") || fail 'could not create temporary directory'
stage=
cleanup() { rm -rf "$tmp"; [ -z "$stage" ] || rm -f "$stage"; }
trap cleanup EXIT HUP INT TERM
ulimit -f 131072 2>/dev/null || fail 'remote shell cannot enforce the 64 MiB file limit'
if command -v curl >/dev/null 2>&1; then
  curl -fsSL --max-filesize 67108864 --max-time 120 -o "$tmp/$archive" "$base/$archive" || fail 'release download failed'
  curl -fsSL --max-time 30 -o "$tmp/$stem.sha256" "$base/$stem.sha256" || fail 'checksum download failed'
elif command -v wget >/dev/null 2>&1; then
  wget -q --timeout=120 -O "$tmp/$archive" "$base/$archive" || fail 'release download failed'
  wget -q --timeout=30 -O "$tmp/$stem.sha256" "$base/$stem.sha256" || fail 'checksum download failed'
else
  fail 'remote host needs curl or wget'
fi
[ "$(wc -c < "$tmp/$archive" | tr -d ' ')" -le 67108864 ] || fail 'release archive exceeds 64 MiB limit'
[ "$(wc -c < "$tmp/$stem.sha256" | tr -d ' ')" -le 4096 ] || fail 'checksum file exceeds 4 KiB limit'
expected=$(awk 'NR == 1 { print $1 }' "$tmp/$stem.sha256")
case "$expected" in ''|*[!0-9A-Fa-f]*) fail 'invalid release checksum' ;; esac
[ "${#expected}" -eq 64 ] || fail 'invalid release checksum'
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$archive" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$archive" | awk '{ print $1 }')
elif command -v openssl >/dev/null 2>&1; then
  actual=$(openssl dgst -sha256 "$tmp/$archive" | awk '{ print $NF }')
else
  fail 'remote host needs sha256sum, shasum, or openssl'
fi
[ "$actual" = "$expected" ] || fail 'release checksum mismatch'
tar -xzf "$tmp/$archive" -C "$tmp" || fail 'release extraction failed'
[ -f "$tmp/luvus" ] || fail 'release archive did not contain luvus'
chmod 755 "$tmp/luvus"
probe=$("$tmp/luvus" fleet-bridge --probe) || fail 'downloaded binary cannot provide fleet control'
printf '%s' "$probe" | grep -F '"protocol":"luvus-fleet-v1"' >/dev/null || fail 'downloaded binary has no fleet protocol'
printf '%s' "$probe" | grep -F "\"version\":\"$version\"" >/dev/null || fail 'downloaded binary version mismatch'
dir=$HOME/.local/bin
mkdir -p "$dir" || fail 'could not create remote install directory'
stage=$(mktemp "$dir/.luvus-machine.XXXXXX") || fail 'could not reserve remote install file'
cp "$tmp/luvus" "$stage" || fail 'could not stage remote binary'
chmod 755 "$stage"
mv -f "$stage" "$dir/luvus" || fail 'could not install remote binary'
printf '%s\n' "$dir/luvus"
"#;

pub(super) fn install(destination: &str) -> Result<String> {
    validate_destination(destination)?;
    let mut command = super::ssh::ssh_command(destination, true);
    command
        .arg("sh")
        .arg("-s")
        .arg("--")
        .arg(env!("CARGO_PKG_VERSION"));
    let output = super::ssh::run_bounded_with_input(
        command,
        PROVISION_TIMEOUT,
        Some(INSTALL_POSIX.as_bytes()),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .next()
            .unwrap_or("remote provisioning failed");
        return Err(anyhow!(
            "could not provision Luvus on `{destination}`: {detail}"
        ));
    }
    let path = String::from_utf8(output.stdout)
        .context("remote install path was not UTF-8")?
        .trim()
        .to_string();
    validate_remote_binary(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn provisioner_is_fixed_verified_and_user_scoped() {
        assert!(INSTALL_POSIX.contains("$HOME/.local/bin"));
        assert!(INSTALL_POSIX.contains("sha256sum"));
        assert!(INSTALL_POSIX.contains("shasum -a 256"));
        assert!(INSTALL_POSIX.contains("openssl dgst -sha256"));
        assert!(INSTALL_POSIX.contains("fleet-bridge --probe"));
        assert!(INSTALL_POSIX.contains("ulimit -f 131072"));
        assert!(INSTALL_POSIX.contains("--max-filesize 67108864"));
        assert!(!INSTALL_POSIX.contains("sudo"));
        assert!(!INSTALL_POSIX.contains("install.sh"));
    }

    #[test]
    fn provision_command_keeps_profile_data_out_of_the_remote_script() {
        let command = {
            let mut command = super::super::ssh::ssh_command("dev@buildbox", true);
            command
                .arg("sh")
                .arg("-s")
                .arg("--")
                .arg(env!("CARGO_PKG_VERSION"));
            command
        };
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args.last().map(String::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(args.iter().filter(|arg| *arg == "dev@buildbox").count(), 1);
        assert!(args.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
    }

    #[cfg(unix)]
    #[test]
    fn provision_script_verifies_and_installs_in_the_user_prefix() {
        let _env = crate::persist::test_env("machine-provision-script");
        let root = crate::persist::config_dir();
        let tools = root.join("tools");
        let home = root.join("home");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let digest = "a".repeat(64);
        write_executable(
            &tools.join("curl"),
            &format!(
                r#"#!/bin/sh
out=
while [ "$#" -gt 0 ]; do
  if [ "$1" = -o ]; then out=$2; shift 2; else shift; fi
done
case "$out" in
  *.sha256) printf '%s  fixture\n' '{digest}' > "$out" ;;
  *) printf 'fixture archive' > "$out" ;;
esac
"#
            ),
        );
        write_executable(
            &tools.join("sha256sum"),
            &format!("#!/bin/sh\nprintf '%s  %s\\n' '{digest}' \"$1\"\n"),
        );
        write_executable(
            &tools.join("tar"),
            &format!(
                r#"#!/bin/sh
dir=
while [ "$#" -gt 0 ]; do
  if [ "$1" = -C ]; then dir=$2; shift 2; else shift; fi
done
cat > "$dir/luvus" <<'LUVUS'
#!/bin/sh
printf '%s\n' '{{"protocol":"luvus-fleet-v1","version":"{}","os":"linux","arch":"x86_64","binary":"/fixture/luvus"}}'
LUVUS
chmod 755 "$dir/luvus"
"#,
                env!("CARGO_PKG_VERSION")
            ),
        );

        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![tools.clone()];
        paths.extend(std::env::split_paths(&path));
        let joined = std::env::join_paths(paths).unwrap();
        let mut command = std::process::Command::new("sh");
        command
            .arg("-s")
            .arg("--")
            .arg(env!("CARGO_PKG_VERSION"))
            .env("HOME", &home)
            .env("PATH", joined);
        let output = super::super::ssh::run_bounded_with_input(
            command,
            Duration::from_secs(3),
            Some(INSTALL_POSIX.as_bytes()),
        )
        .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            home.join(".local/bin/luvus").to_string_lossy()
        );
        assert!(home.join(".local/bin/luvus").is_file());
    }
}
