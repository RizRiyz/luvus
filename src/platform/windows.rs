//! Windows process identity and bounded process-tree inspection.
//!
//! Keep native calls here so the rest of Luvus can use the same platform
//! contract as macOS/Linux without carrying Windows handles through app state.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::size_of;

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, PEB,
    PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ, RTL_USER_PROCESS_PARAMETERS,
};

const MAX_PROCESS_ENTRIES: usize = 16_384;
const MAX_DESCENDANTS_PER_ROOT: usize = 64;
const MAX_COMMAND_LINE_BYTES: usize = 64 * 1024;

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Option<Self> {
        (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self(handle))
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `OwnedHandle` is constructed only from a successful Win32
        // handle-returning call and owns that handle exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    // SAFETY: the access mask is read-only and `pid` is passed by value.
    OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })
}

fn read_process_memory<T: Default>(process: HANDLE, address: *const c_void) -> Option<T> {
    let mut value = T::default();
    let mut bytes_read = 0;
    // SAFETY: `address` is supplied by the target process and is only read via
    // the OS API; `value` is writable storage of the exact requested size.
    let success = unsafe {
        ReadProcessMemory(
            process,
            address,
            (&mut value as *mut T).cast(),
            size_of::<T>(),
            &mut bytes_read,
        )
    } != 0;
    (success && bytes_read == size_of::<T>()).then_some(value)
}

/// Read a process's full command line from its PEB.
///
/// Windows' ToolHelp snapshot exposes only the executable name. The command
/// line is needed for runtimes such as `node ...\\pi-coding-agent\\...`, where
/// the agent identity lives in a script argument rather than the executable.
/// This is best-effort: protected, exited, or differently-bitness processes
/// fall back to their executable name at the snapshot caller.
fn process_command_line(pid: u32) -> Option<String> {
    let process = OwnedHandle::new(unsafe {
        OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid)
    })?;
    let mut basic_info = PROCESS_BASIC_INFORMATION::default();
    let mut return_length = 0_u32;
    // SAFETY: `basic_info` is writable storage of the documented result size.
    let status = unsafe {
        NtQueryInformationProcess(
            process.0,
            ProcessBasicInformation,
            (&mut basic_info as *mut PROCESS_BASIC_INFORMATION).cast(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut return_length,
        )
    };
    if status < 0 || basic_info.PebBaseAddress.is_null() {
        return None;
    }
    let peb = read_process_memory::<PEB>(process.0, basic_info.PebBaseAddress.cast())?;
    if peb.ProcessParameters.is_null() {
        return None;
    }
    let parameters = read_process_memory::<RTL_USER_PROCESS_PARAMETERS>(
        process.0,
        peb.ProcessParameters.cast(),
    )?;
    let command_line = parameters.CommandLine;
    let length = usize::from(command_line.Length);
    if length == 0 || length % size_of::<u16>() != 0 || length > MAX_COMMAND_LINE_BYTES {
        return None;
    }
    if command_line.Buffer.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; length / size_of::<u16>()];
    let mut bytes_read = 0;
    // SAFETY: the target buffer and length come from the target's live process
    // parameters; the destination is allocated for exactly `length` bytes.
    let success = unsafe {
        ReadProcessMemory(
            process.0,
            command_line.Buffer.cast(),
            buffer.as_mut_ptr().cast(),
            length,
            &mut bytes_read,
        )
    } != 0;
    if !success || bytes_read != length {
        return None;
    }
    String::from_utf16(&buffer).ok()
}

fn token_user(process: HANDLE) -> Option<Vec<usize>> {
    let mut token = std::ptr::null_mut();
    // SAFETY: `token` points to writable storage and the requested access is
    // read-only. The returned token handle is owned below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let token = OwnedHandle::new(token)?;
    let mut needed = 0_u32;
    // The first call intentionally supplies no buffer to obtain its exact size.
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed < size_of::<TOKEN_USER>() as u32 || needed > 64 * 1024 {
        return None;
    }
    // TOKEN_USER contains a pointer and must be read from suitably aligned
    // storage. A byte vector is not guaranteed to provide that alignment.
    let words = (needed as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    // SAFETY: the buffer is exactly the size requested by Windows and remains
    // alive for every later `TOKEN_USER`/SID access.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return None;
    }
    Some(buffer)
}

/// Confirm that `pid` belongs to the same Windows account as this process.
/// Used by named-pipe clients before trusting a discovered server endpoint.
pub(super) fn process_belongs_to_current_user(pid: u32) -> bool {
    let Some(process) = open_process(pid) else {
        return false;
    };
    // SAFETY: `GetCurrentProcess` returns a valid pseudo-handle for this process.
    let Some(current) = token_user(unsafe { GetCurrentProcess() }) else {
        return false;
    };
    let Some(peer) = token_user(process.0) else {
        return false;
    };
    // SAFETY: both buffers were populated as TOKEN_USER and remain alive.
    let current = unsafe { &*current.as_ptr().cast::<TOKEN_USER>() };
    let peer = unsafe { &*peer.as_ptr().cast::<TOKEN_USER>() };
    // SAFETY: both SID pointers are owned by the live token-information buffers.
    unsafe { EqualSid(current.User.Sid, peer.User.Sid) != 0 }
}

/// PID-reuse-safe process lifetime marker, expressed as the opaque Windows
/// creation timestamp in 100-nanosecond ticks.
pub(super) fn process_start_marker(pid: u32) -> Option<String> {
    let process = open_process(pid)?;
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all output pointers name initialized writable FILETIME values and
    // the process handle was opened for query-only access.
    if unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0
    {
        return None;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Some(format!("windows:{ticks}"))
}

#[derive(Clone, Debug)]
struct ProcessEntry {
    pid: u32,
    parent: u32,
    executable: String,
}

#[derive(Debug)]
struct ProcessSnapshot {
    names: HashMap<u32, String>,
    children: HashMap<u32, Vec<u32>>,
    command_lines: HashMap<u32, String>,
}

impl ProcessSnapshot {
    fn capture() -> Option<Self> {
        // SAFETY: ToolHelp owns the returned snapshot handle; the guard closes it.
        let snapshot =
            OwnedHandle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
        let mut raw = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut entries = Vec::new();
        // SAFETY: `raw` has the required size and remains writable for the loop.
        let mut available = unsafe { Process32FirstW(snapshot.0, &mut raw) } != 0;
        while available && entries.len() < MAX_PROCESS_ENTRIES {
            let end = raw
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(raw.szExeFile.len());
            let executable = String::from_utf16_lossy(&raw.szExeFile[..end]);
            if raw.th32ProcessID != 0 && !executable.is_empty() {
                entries.push(ProcessEntry {
                    pid: raw.th32ProcessID,
                    parent: raw.th32ParentProcessID,
                    executable,
                });
            }
            // SAFETY: same initialized ToolHelp snapshot and writable entry.
            available = unsafe { Process32NextW(snapshot.0, &mut raw) } != 0;
        }
        if entries.is_empty() {
            return None;
        }
        Some(Self::from_entries(entries))
    }

    fn from_entries(entries: Vec<ProcessEntry>) -> Self {
        let mut names = HashMap::with_capacity(entries.len());
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for entry in entries {
            names.insert(entry.pid, entry.executable);
            children.entry(entry.parent).or_default().push(entry.pid);
        }
        for child_ids in children.values_mut() {
            child_ids.sort_unstable();
        }
        Self {
            names,
            children,
            command_lines: HashMap::new(),
        }
    }

    fn descendants(&self, root: u32) -> Vec<(u32, u16)> {
        let mut output = Vec::new();
        let mut pending = vec![(root, 0_u16)];
        let mut visited = HashSet::new();
        while let Some((pid, depth)) = pending.pop() {
            if !visited.insert(pid) || output.len() >= MAX_DESCENDANTS_PER_ROOT {
                continue;
            }
            if self.names.contains_key(&pid) {
                output.push((pid, depth));
            }
            if let Some(children) = self.children.get(&pid) {
                pending.extend(
                    children
                        .iter()
                        .rev()
                        .copied()
                        .map(|child| (child, depth.saturating_add(1))),
                );
            }
        }
        output
    }

    fn command(&mut self, pid: u32) -> String {
        if let Some(command) = self.command_lines.get(&pid) {
            return command.clone();
        }
        let fallback = self.names.get(&pid).cloned().unwrap_or_default();
        let command = process_command_line(pid).unwrap_or(fallback);
        self.command_lines.insert(pid, command.clone());
        command
    }
}

pub(super) fn descendant_commands(roots: &[u32]) -> Option<HashMap<u32, Vec<String>>> {
    let mut snapshot = ProcessSnapshot::capture()?;
    Some(
        roots
            .iter()
            .copied()
            .map(|root| {
                let commands = snapshot
                    .descendants(root)
                    .into_iter()
                    .map(|(pid, _)| snapshot.command(pid))
                    .collect();
                (root, commands)
            })
            .collect(),
    )
}

pub(super) fn process_tree(root: u32) -> Vec<super::ProcInfo> {
    ProcessSnapshot::capture()
        .map(|mut snapshot| {
            snapshot
                .descendants(root)
                .into_iter()
                .map(|(pid, depth)| super::ProcInfo {
                    pid,
                    depth,
                    command: snapshot.command(pid),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descendant_walk_is_bounded_and_cycle_safe() {
        let mut entries = vec![ProcessEntry {
            pid: 1,
            parent: 65,
            executable: "root.exe".into(),
        }];
        entries.extend((2..=65).map(|pid| ProcessEntry {
            pid,
            parent: pid - 1,
            executable: format!("child-{pid}.exe"),
        }));
        let descendants = ProcessSnapshot::from_entries(entries).descendants(1);
        assert_eq!(descendants.len(), MAX_DESCENDANTS_PER_ROOT);
        assert_eq!(descendants[0], (1, 0));
    }

    #[test]
    fn current_process_has_a_stable_identity_and_owner() {
        let pid = std::process::id();
        assert!(process_belongs_to_current_user(pid));
        let first = process_start_marker(pid).expect("Windows process creation time");
        assert_eq!(process_start_marker(pid).as_deref(), Some(first.as_str()));
        assert!(first.starts_with("windows:"));
    }

    #[test]
    fn process_snapshot_contains_the_current_process() {
        let pid = std::process::id();
        let mut snapshot = ProcessSnapshot::capture().expect("Windows ToolHelp snapshot");
        let root = snapshot.descendants(pid);
        assert_eq!(root.first().map(|entry| entry.0), Some(pid));
        assert!(!snapshot.command(pid).is_empty());
    }

    #[test]
    fn process_command_line_includes_arguments() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping.exe -n 3 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn cmd");
        let commands = descendant_commands(&[child.id()]).expect("capture process tree");
        let command = commands
            .get(&child.id())
            .and_then(|commands| commands.first())
            .expect("root command line");
        assert!(command.to_ascii_lowercase().contains("/c"), "{command:?}");
        let _ = child.kill();
        let _ = child.wait();
    }
}
