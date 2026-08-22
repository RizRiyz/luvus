//! Bounded, off-loop file-path discovery for the global finder (docs/90).

use std::collections::HashSet;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{FILE_BYTES_CAP, FILE_COUNT_CAP};

#[derive(Clone, Debug)]
pub struct FileRecord {
    pub path: PathBuf,
    pub relative: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct FileCatalog {
    pub records: Vec<FileRecord>,
    pub truncated: bool,
    pub partial: bool,
}

const CACHE_TTL: Duration = Duration::from_secs(2);
const CACHE_BYTES_CAP: usize = 32 * 1024 * 1024;
const GIT_INDEX_TIMEOUT: Duration = Duration::from_secs(2);

struct CacheEntry {
    root: PathBuf,
    at: Instant,
    bytes: usize,
    catalog: Arc<FileCatalog>,
}

static CACHE: OnceLock<Mutex<Vec<CacheEntry>>> = OnceLock::new();

fn cached_catalog(
    entries: &mut Vec<CacheEntry>,
    key: &Path,
    now: Instant,
) -> Option<Arc<FileCatalog>> {
    entries.retain(|entry| now.duration_since(entry.at) <= CACHE_TTL);
    entries
        .iter()
        .find(|entry| entry.root == key)
        .map(|entry| Arc::clone(&entry.catalog))
}

pub fn index(root: &Path) -> FileCatalog {
    git_files(root).unwrap_or_else(|| walk_files(root))
}

/// Reuse a recent path-only catalog across interactive and API queries. The
/// short TTL keeps changes fresh while preventing CLI federation from running
/// `git ls-files` for every keystroke. Entries obey a global byte cap.
pub fn index_cached(root: &Path) -> Arc<FileCatalog> {
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut entries) = cache.lock() {
        if let Some(catalog) = cached_catalog(&mut entries, &key, Instant::now()) {
            return catalog;
        }
    }

    let catalog = Arc::new(index(root));
    let bytes = catalog
        .records
        .iter()
        .map(|record| record.path.as_os_str().len() + record.relative.as_os_str().len())
        .sum::<usize>();
    if let Ok(mut entries) = cache.lock() {
        entries.retain(|entry| entry.root != key);
        entries.push(CacheEntry {
            root: key,
            at: Instant::now(),
            bytes,
            catalog: Arc::clone(&catalog),
        });
        while entries.iter().map(|entry| entry.bytes).sum::<usize>() > CACHE_BYTES_CAP {
            let Some((oldest, _)) = entries.iter().enumerate().min_by_key(|(_, entry)| entry.at)
            else {
                break;
            };
            entries.remove(oldest);
        }
    }
    catalog
}

fn git_files(root: &Path) -> Option<FileCatalog> {
    let deadline = Instant::now() + GIT_INDEX_TIMEOUT;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let _ = tx.send(read_git_output(stdout));
    });
    let (bytes, mut truncated) =
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(output)) => output,
            Ok(Err(_)) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        };
    if truncated {
        let _ = child.kill();
    }
    let _ = reader.join();
    let status = if truncated {
        child.wait().ok()?
    } else {
        wait_for_child(&mut child, deadline)?
    };
    if !status.success() && !truncated {
        return None;
    }
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    for raw in bytes.split(|b| *b == 0).filter(|raw| !raw.is_empty()) {
        if records.len() >= FILE_COUNT_CAP {
            truncated = true;
            break;
        }
        let relative = path_from_bytes(raw);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        if seen.insert(relative.clone()) {
            records.push(FileRecord {
                path: root.join(&relative),
                relative,
            });
        }
    }
    Some(FileCatalog {
        records,
        truncated,
        partial: false,
    })
}

fn wait_for_child(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                return child.wait().ok();
            }
        }
    }
}

fn read_git_output(mut stdout: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = stdout.read(&mut chunk)?;
        if n == 0 {
            return Ok((bytes, false));
        }
        let room = FILE_BYTES_CAP.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..n.min(room)]);
        if n > room || bytes.len() >= FILE_BYTES_CAP {
            return Ok((bytes, true));
        }
    }
}

fn walk_files(root: &Path) -> FileCatalog {
    let mut catalog = FileCatalog::default();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut bytes = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if depth > 64 {
            catalog.truncated = true;
            continue;
        }
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            Err(_) => {
                catalog.partial = true;
                continue;
            }
        };
        let mut entries = Vec::new();
        for entry in read {
            match entry {
                Ok(entry) => entries.push(entry),
                Err(_) => catalog.partial = true,
            }
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => {
                    catalog.partial = true;
                    continue;
                }
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root).map(Path::to_path_buf) else {
                continue;
            };
            bytes = bytes.saturating_add(relative.as_os_str().len());
            if bytes > FILE_BYTES_CAP || catalog.records.len() >= FILE_COUNT_CAP {
                catalog.truncated = true;
                return catalog;
            }
            catalog.records.push(FileRecord { path, relative });
        }
    }
    catalog.records.sort_by(|a, b| a.relative.cmp(&b.relative));
    catalog
}

#[cfg(unix)]
fn path_from_bytes(raw: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(raw.to_vec()))
}

#[cfg(not(unix))]
fn path_from_bytes(raw: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(raw).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_walk_skips_git_and_directory_symlinks() {
        let root = std::env::temp_dir().join(format!("luvus-search-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join(".git/secret"), "hidden").unwrap();
        let catalog = walk_files(&root);
        assert_eq!(catalog.records.len(), 1);
        assert_eq!(catalog.records[0].relative, PathBuf::from("src/main.rs"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recent_catalog_is_reused() {
        let root = std::env::temp_dir().join(format!(
            "luvus-search-cache-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.txt"), "one").unwrap();
        let first = index_cached(&root);
        let second = index_cached(&root);
        assert!(Arc::ptr_eq(&first, &second));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_ttl_is_measured_from_insertion_not_last_hit() {
        let inserted = Instant::now();
        let key = PathBuf::from("workspace");
        let catalog = Arc::new(FileCatalog::default());
        let mut entries = vec![CacheEntry {
            root: key.clone(),
            at: inserted,
            bytes: 0,
            catalog: Arc::clone(&catalog),
        }];

        let hit = cached_catalog(&mut entries, &key, inserted + CACHE_TTL / 2).unwrap();
        assert!(Arc::ptr_eq(&hit, &catalog));
        assert!(cached_catalog(
            &mut entries,
            &key,
            inserted + CACHE_TTL + Duration::from_millis(1)
        )
        .is_none());
    }

    #[test]
    fn git_output_reader_caps_bytes_without_waiting_for_eof() {
        let data = vec![b'x'; FILE_BYTES_CAP + 1];
        let (bytes, truncated) = read_git_output(std::io::Cursor::new(data)).unwrap();
        assert_eq!(bytes.len(), FILE_BYTES_CAP);
        assert!(truncated);
    }
}
