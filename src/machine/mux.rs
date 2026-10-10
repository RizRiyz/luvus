//! One shared SSH connection per saved-machine destination.
//!
//! Preparing a machine runs several short SSH commands (discovery, target
//! detection, installation, endpoint checks) and the persistent link may
//! reconnect. Opening a new TCP connection for each one trips common server
//! rate limits such as UFW `limit` (six connections in 30 seconds), which then
//! refuses the step that matters. OpenSSH connection sharing sends every
//! command over one authenticated connection instead.
//!
//! Anyone who can reach a control socket can run commands as the user, so the
//! socket directory must be private: created `0700`, owned by this user, and
//! never a symlink. Without such a directory, commands connect directly as
//! before. Windows OpenSSH does not support connection sharing.

use std::process::Command;

#[cfg(unix)]
use std::path::{Path, PathBuf};

/// How long an idle shared connection stays open after its last command.
#[cfg(unix)]
const CONTROL_PERSIST_SECONDS: u32 = 60;

/// macOS allows 103 bytes plus the terminating NUL in `sun_path`. OpenSSH
/// names the socket `%C` (a 40-character hash) and binds it first under a
/// temporary name with a 17-byte random suffix.
#[cfg(unix)]
const MAX_SOCKET_PATH_BYTES: usize = 103;
#[cfg(unix)]
const SOCKET_NAME_BYTES: usize = 1 + 40 + 17;

/// Add connection-sharing options to an `ssh` command, before its destination.
#[cfg(unix)]
pub(super) fn apply(command: &mut Command) {
    if let Some(dir) = control_dir() {
        command.args(options(&dir));
    }
}

#[cfg(not(unix))]
pub(super) fn apply(_command: &mut Command) {}

#[cfg(unix)]
fn options(dir: &Path) -> [String; 6] {
    [
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}/%C", dir.display()),
        "-o".into(),
        format!("ControlPersist={CONTROL_PERSIST_SECONDS}"),
    ]
}

/// The private socket directory: `<luvus home>/ssh`, or an owner-scoped
/// temporary directory when the home is too long for a socket path.
#[cfg(unix)]
fn control_dir() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let preferred = crate::persist::config_dir().join("ssh");
    if usable_path(&preferred) {
        return private_dir(&preferred, uid).then_some(preferred);
    }
    #[cfg(target_os = "macos")]
    let temporary_root = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    let temporary_root = "/tmp";
    let parent = PathBuf::from(format!("{temporary_root}/luvus-{uid}"));
    let fallback = parent.join("ssh");
    (usable_path(&fallback) && private_dir(&parent, uid) && private_dir(&fallback, uid))
        .then_some(fallback)
}

/// Short enough for a socket path, and safe inside one `-o` option value:
/// OpenSSH splits option values on whitespace and expands `%` tokens.
#[cfg(unix)]
fn usable_path(dir: &Path) -> bool {
    let Some(text) = dir.to_str() else {
        return false;
    };
    dir.is_absolute()
        && text.len() + SOCKET_NAME_BYTES <= MAX_SOCKET_PATH_BYTES
        && !text
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '%' | '"' | '\''))
}

/// Create `dir` if needed and confirm it is a real directory owned by `uid`
/// that no one else can enter. A wider mode on our own directory is narrowed.
#[cfg(unix)]
fn private_dir(dir: &Path, uid: u32) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    let Ok(metadata) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if !metadata.file_type().is_dir() || metadata.uid() != uid {
        return false;
    }
    if metadata.mode() & 0o077 != 0
        && std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err()
    {
        return false;
    }
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn shared_connections_use_a_private_socket_directory() {
        let _env = crate::persist::test_env("machine-mux");
        let dir = control_dir().expect("a private control directory");
        let metadata = std::fs::symlink_metadata(&dir).unwrap();
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert!(dir.as_os_str().len() + SOCKET_NAME_BYTES <= MAX_SOCKET_PATH_BYTES);

        let options = options(&dir);
        assert_eq!(options[1], "ControlMaster=auto");
        assert_eq!(options[3], format!("ControlPath={}/%C", dir.display()));
        assert_eq!(options[5], "ControlPersist=60");
    }

    #[test]
    fn a_long_or_unsafe_home_is_not_used_for_sockets() {
        assert!(usable_path(Path::new("/Users/me/.luvus/ssh")));
        assert!(!usable_path(Path::new(&format!("/{}/ssh", "a".repeat(60)))));
        assert!(!usable_path(Path::new("/Users/Jane Doe/.luvus/ssh")));
        assert!(!usable_path(Path::new("/tmp/100%/ssh")));
        assert!(!usable_path(Path::new("relative/ssh")));
    }

    #[test]
    fn a_socket_directory_owned_by_someone_else_or_linked_is_refused() {
        let root = std::env::temp_dir().join(format!("luvus-mux-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let uid = unsafe { libc::geteuid() };

        let open = root.join("open");
        std::fs::create_dir(&open).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(private_dir(&open, uid), "our own directory is narrowed");
        assert_eq!(
            std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let link = root.join("link");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        assert!(!private_dir(&link, uid), "a symlink is never trusted");
        assert!(
            !private_dir(&open, uid.wrapping_add(1)),
            "another owner is refused"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
