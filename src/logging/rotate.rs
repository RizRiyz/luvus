use std::fs;
use std::io::{self, Write};

use fs2::FileExt;

use super::{path, LoggerKind};

pub(crate) const MAX_FILE_BYTES: u64 = 3 * 1024 * 1024;

pub(crate) fn append_records(kind: LoggerKind, records: &[Vec<u8>]) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    path::prepare_log_dir()?;
    let lock = path::open_private_append(&path::lock_path(kind))?;
    lock.lock_exclusive()?;
    let result = append_locked(kind, records);
    let _ = FileExt::unlock(&lock);
    result
}

fn append_locked(kind: LoggerKind, records: &[Vec<u8>]) -> io::Result<()> {
    let current = path::log_path(kind);
    let archived = current.with_extension("log.1");
    for record in records {
        let record_len = u64::try_from(record.len()).unwrap_or(u64::MAX);
        let mut file = path::open_private_append(&current)?;
        let len = file.metadata()?.len();
        if len > 0 && len.saturating_add(record_len) > MAX_FILE_BYTES {
            drop(file);
            rotate(&current, &archived)?;
            file = path::open_private_append(&current)?;
        }
        file.write_all(record)?;
    }
    Ok(())
}

fn rotate(current: &std::path::Path, archived: &std::path::Path) -> io::Result<()> {
    for path in [current, archived] {
        path::reject_existing_unsafe(path)?;
    }
    if archived.exists() {
        fs::remove_file(archived)?;
    }
    if current.exists() {
        fs::rename(current, archived)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_record_boundaries_and_keeps_one_archive() {
        let _env = crate::persist::test_env("logging-rotate");
        let nearly_full = vec![b'x'; MAX_FILE_BYTES as usize - 2];
        append_records(LoggerKind::Server, &[nearly_full]).unwrap();
        append_records(LoggerKind::Server, &[b"a\n".to_vec(), b"b\n".to_vec()]).unwrap();
        let path = path::log_path(LoggerKind::Server);
        assert_eq!(fs::read(&path).unwrap(), b"b\n");
        assert_eq!(
            fs::metadata(path.with_extension("log.1")).unwrap().len(),
            MAX_FILE_BYTES
        );
        assert!(!path.with_extension("log.2").exists());
    }

    #[test]
    fn independent_appenders_keep_complete_lines() {
        let _env = crate::persist::test_env("logging-concurrent");
        let first = std::thread::spawn(|| {
            for _ in 0..100 {
                append_records(LoggerKind::Client, &[b"{\"writer\":1}\n".to_vec()]).unwrap();
            }
        });
        let second = std::thread::spawn(|| {
            for _ in 0..100 {
                append_records(LoggerKind::Client, &[b"{\"writer\":2}\n".to_vec()]).unwrap();
            }
        });
        first.join().unwrap();
        second.join().unwrap();
        let text = fs::read_to_string(path::log_path(LoggerKind::Client)).unwrap();
        assert_eq!(text.lines().count(), 200);
        assert!(text
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));
    }
}
