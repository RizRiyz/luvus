use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use super::LoggerKind;

pub(crate) fn log_dir() -> PathBuf {
    crate::session::session_dir_for(crate::session::active_name().as_deref()).join("logs")
}

pub(crate) fn log_path(kind: LoggerKind) -> PathBuf {
    log_dir().join(match kind {
        LoggerKind::Server => "server.log",
        LoggerKind::Client => "client.log",
    })
}

pub(crate) fn lock_path(kind: LoggerKind) -> PathBuf {
    log_dir().join(match kind {
        LoggerKind::Server => "server.lock",
        LoggerKind::Client => "client.lock",
    })
}

pub(crate) fn prepare_log_dir() -> io::Result<PathBuf> {
    let dir = log_dir();
    if let Ok(metadata) = fs::symlink_metadata(&dir) {
        ensure_real_dir(&metadata)?;
    } else {
        fs::create_dir_all(&dir)?;
        ensure_real_dir(&fs::symlink_metadata(&dir)?)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn ensure_real_dir(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() || is_reparse_point(metadata) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log directory is not a real directory",
        ))
    } else {
        Ok(())
    }
}

fn ensure_regular(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() || is_reparse_point(metadata) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log path is not a regular file",
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

pub(crate) fn open_private_append(path: &Path) -> io::Result<File> {
    reject_existing_unsafe(path)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    ensure_regular(&file.metadata()?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

pub(crate) fn reject_existing_unsafe(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure_regular(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn nearest_existing_parent(mut path: &Path) -> Option<&Path> {
    loop {
        if path.exists() {
            return Some(path);
        }
        path = path.parent()?;
    }
}

pub(crate) fn directory_writable_without_creating(path: &Path) -> bool {
    let Some(parent) = nearest_existing_parent(path) else {
        return false;
    };
    // If the requested log directory already exists, mirror
    // `prepare_log_dir`: files, symlinks, and Windows reparse points are not
    // usable log directories even when the object itself is writable.
    if parent == path {
        let Ok(metadata) = fs::symlink_metadata(parent) else {
            return false;
        };
        if ensure_real_dir(&metadata).is_err() {
            return false;
        }
    } else if !parent.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = CString::new(parent.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `path` is a valid NUL-terminated byte sequence for `access`.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ADD_FILE, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let wide: Vec<u16> = parent.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the UTF-16 path is NUL-terminated and the handle is closed below.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_ADD_FILE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            false
        } else {
            // SAFETY: `handle` is valid and owned by this function.
            unsafe { CloseHandle(handle) };
            true
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        !parent
            .metadata()
            .map(|metadata| metadata.permissions().readonly())
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_paths_follow_the_selected_test_home() {
        let _env = crate::persist::test_env("logging-path");
        assert_eq!(log_path(LoggerKind::Server), log_dir().join("server.log"));
        assert!(!log_dir().exists());
    }

    #[test]
    fn existing_non_directory_is_not_reported_writable() {
        let _env = crate::persist::test_env("logging-health-file");
        let dir = log_dir();
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::write(&dir, b"not a directory").unwrap();

        assert!(!directory_writable_without_creating(&dir));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_log_files() {
        use std::os::unix::fs::symlink;
        let _env = crate::persist::test_env("logging-symlink");
        let dir = prepare_log_dir().unwrap();
        let target = dir.join("target");
        fs::write(&target, b"private").unwrap();
        let path = log_path(LoggerKind::Server);
        symlink(&target, &path).unwrap();
        assert!(open_private_append(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlink_log_directory_is_not_reported_writable() {
        use std::os::unix::fs::symlink;
        let _env = crate::persist::test_env("logging-health-symlink");
        let dir = log_dir();
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        let target = dir.with_file_name("actual-logs");
        fs::create_dir(&target).unwrap();
        symlink(&target, &dir).unwrap();

        assert!(!directory_writable_without_creating(&dir));
    }

    #[cfg(unix)]
    #[test]
    fn writer_paths_are_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::persist::test_env("logging-permissions");
        let dir = prepare_log_dir().unwrap();
        let path = log_path(LoggerKind::Server);
        let file = open_private_append(&path).unwrap();
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}
