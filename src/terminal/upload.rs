//! Bounded terminal file uploads owned by one authenticated control stream.
//!
//! Browsers send small base64 chunks; Luvus writes them directly into private
//! selected-session storage and pastes only the completed server-side path.
//! An interrupted stream drops its partial file without touching completed
//! uploads or user files.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CHUNK_BYTES: usize = 160 * 1024;
pub const MAX_CHUNK_BASE64_BYTES: usize = MAX_CHUNK_BYTES.div_ceil(3) * 4;

const PREFIX: &str = "luvus-upload-";
const PART_PREFIX: &str = ".luvus-upload-";
const PART_SUFFIX: &str = ".part";
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_STAGING_SCAN: usize = 1024;
const MAX_FRESH_UPLOADS: usize = 128;

pub struct UploadState {
    active: Option<ActiveUpload>,
}

struct ActiveUpload {
    id: String,
    expected: usize,
    received: usize,
    part_path: PathBuf,
    final_path: PathBuf,
    file: File,
}

impl UploadState {
    pub fn new() -> Self {
        Self { active: None }
    }

    pub fn start(&mut self, name: &str, size: usize) -> io::Result<String> {
        if self.active.is_some() {
            return Err(invalid("another terminal upload is already active"));
        }
        if size == 0 || size > MAX_UPLOAD_BYTES {
            return Err(invalid("terminal upload size is outside the allowed range"));
        }
        let dir = crate::persist::ensure_terminal_upload_dir()?;
        cleanup_staged(&dir);
        let safe_name = safe_file_name(name);
        for _ in 0..8 {
            let id = crate::terminal::backend::random_id().map_err(io::Error::other)?;
            let part_path = dir.join(format!(".{PREFIX}{id}{PART_SUFFIX}"));
            let final_path = dir.join(format!("{PREFIX}{id}-{safe_name}"));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&part_path) {
                Ok(file) => {
                    self.active = Some(ActiveUpload {
                        id: id.clone(),
                        expected: size,
                        received: 0,
                        part_path,
                        final_path,
                        file,
                    });
                    return Ok(id);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique terminal upload",
        ))
    }

    pub fn append(&mut self, id: &str, offset: usize, encoded: &str) -> io::Result<usize> {
        let active = self
            .active
            .as_mut()
            .filter(|active| active.id == id)
            .ok_or_else(|| invalid("terminal upload is not active"))?;
        if offset != active.received {
            return Err(invalid("terminal upload offset does not match"));
        }
        let bytes = decode_chunk(encoded)?;
        if bytes.is_empty()
            || active.received.saturating_add(bytes.len()) > active.expected
            || bytes.len() > MAX_CHUNK_BYTES
        {
            return Err(invalid("terminal upload chunk exceeds the declared size"));
        }
        active.file.write_all(&bytes)?;
        active.received += bytes.len();
        Ok(active.received)
    }

    pub fn finish(&mut self, id: &str) -> io::Result<PathBuf> {
        let matches = self.active.as_ref().is_some_and(|active| active.id == id);
        if !matches {
            return Err(invalid("terminal upload is not active"));
        }
        if self.active.as_ref().map(|active| active.received)
            != self.active.as_ref().map(|active| active.expected)
        {
            return Err(invalid("terminal upload is incomplete"));
        }
        let mut active = self.active.take().expect("active upload was checked");
        active.file.flush()?;
        active.file.sync_data()?;
        drop(active.file);
        if let Err(error) = fs::rename(&active.part_path, &active.final_path) {
            let _ = fs::remove_file(&active.part_path);
            return Err(error);
        }
        Ok(active.final_path)
    }

    pub fn cancel(&mut self, id: &str) -> bool {
        if !self.active.as_ref().is_some_and(|active| active.id == id) {
            return false;
        }
        if let Some(active) = self.active.take() {
            drop(active.file);
            let _ = fs::remove_file(active.part_path);
        }
        true
    }
}

impl Drop for UploadState {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            drop(active.file);
            let _ = fs::remove_file(active.part_path);
        }
    }
}

pub fn discard_completed(path: &Path) {
    let expected_dir = crate::persist::session_dir().join("terminal-uploads");
    let owned = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(PREFIX) && !name.ends_with(PART_SUFFIX));
    if path.parent() == Some(expected_dir.as_path()) && owned {
        let _ = fs::remove_file(path);
    }
}

fn safe_file_name(name: &str) -> String {
    let leaf = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim_matches('.');
    let mut safe = String::with_capacity(leaf.len().min(96));
    let mut underscore = false;
    for character in leaf.chars() {
        if safe.len() >= 96 {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
            safe.push(character);
            underscore = false;
        } else if !underscore {
            safe.push('_');
            underscore = true;
        }
    }
    let safe = safe.trim_matches(['.', '_']);
    if safe.is_empty() {
        "attachment".into()
    } else {
        safe.into()
    }
}

fn decode_chunk(encoded: &str) -> io::Result<Vec<u8>> {
    if encoded.is_empty()
        || encoded.len() > MAX_CHUNK_BASE64_BYTES
        || !encoded.len().is_multiple_of(4)
    {
        return Err(invalid("invalid terminal upload encoding"));
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
    let (chunks, remainder) = encoded.as_bytes().as_chunks::<4>();
    debug_assert!(remainder.is_empty());
    for (index, chunk) in chunks.iter().enumerate() {
        let last = (index + 1) * 4 == encoded.len();
        let a = base64_value(chunk[0]).ok_or_else(|| invalid("invalid base64"))?;
        let b = base64_value(chunk[1]).ok_or_else(|| invalid("invalid base64"))?;
        let c = if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' || b & 0x0f != 0 {
                return Err(invalid("invalid base64 padding"));
            }
            None
        } else {
            Some(base64_value(chunk[2]).ok_or_else(|| invalid("invalid base64"))?)
        };
        let d = if c.is_none() {
            None
        } else if chunk[3] == b'=' {
            if !last || c.unwrap_or_default() & 0x03 != 0 {
                return Err(invalid("invalid base64 padding"));
            }
            None
        } else {
            Some(base64_value(chunk[3]).ok_or_else(|| invalid("invalid base64"))?)
        };
        decoded.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            decoded.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                decoded.push((c << 6) | d);
            }
        }
    }
    (decoded.len() <= MAX_CHUNK_BYTES)
        .then_some(decoded)
        .ok_or_else(|| invalid("terminal upload chunk exceeds the limit"))
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn cleanup_staged(dir: &Path) {
    let now = SystemTime::now();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut fresh = Vec::new();
    for entry in entries.take(MAX_STAGING_SCAN).flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let partial = name.starts_with(PART_PREFIX) && name.ends_with(PART_SUFFIX);
        let completed = name.starts_with(PREFIX) && !name.ends_with(PART_SUFFIX);
        if !partial && !completed {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if now
            .duration_since(modified)
            .is_ok_and(|age| age >= STALE_AFTER)
        {
            let _ = fs::remove_file(path);
        } else if completed {
            fresh.push((modified, path));
        }
    }
    if fresh.len() >= MAX_FRESH_UPLOADS {
        fresh.sort_by_key(|(modified, _)| *modified);
        let remove = fresh.len() - (MAX_FRESH_UPLOADS - 1);
        for (_, path) in fresh.into_iter().take(remove) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_is_ordered_private_atomic_and_disconnect_safe() {
        let _env = crate::persist::test_env("terminal-upload");
        crate::persist::ensure_session_dir();
        let mut upload = UploadState::new();
        let id = upload.start("../../hello world.txt", 5).unwrap();
        assert!(upload.append(&id, 1, "aGVsbG8=").is_err());
        assert_eq!(upload.append(&id, 0, "aGVsbG8=").unwrap(), 5);
        let path = upload.finish(&id).unwrap();
        assert_eq!(
            path.parent().unwrap(),
            crate::persist::session_dir().join("terminal-uploads")
        );
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("hello_world.txt"));
        assert_eq!(fs::read(&path).unwrap(), b"hello");

        let id = upload.start("unfinished.bin", 4).unwrap();
        let part = upload.active.as_ref().unwrap().part_path.clone();
        assert_eq!(upload.append(&id, 0, "eA==").unwrap(), 1);
        drop(upload);
        assert!(!part.exists());
        assert!(path.exists());
    }

    #[test]
    fn upload_rejects_invalid_sizes_chunks_and_padding() {
        let _env = crate::persist::test_env("terminal-upload-invalid");
        crate::persist::ensure_session_dir();
        let mut upload = UploadState::new();
        assert!(upload.start("empty", 0).is_err());
        assert!(upload.start("large", MAX_UPLOAD_BYTES + 1).is_err());
        let id = upload.start("four.bin", 4).unwrap();
        for invalid in ["abc", "####", "Zg=A", "Zg==\n"] {
            assert!(upload.append(&id, 0, invalid).is_err(), "{invalid:?}");
        }
        assert!(upload.append(&id, 0, "aGVsbG8=").is_err());
        assert!(upload.cancel(&id));
    }
}
