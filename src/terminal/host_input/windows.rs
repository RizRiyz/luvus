//! Windows console input setup and record reader.

use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{poll, read, Event};
use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleInputW, SetConsoleMode,
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_INPUT, FOCUS_EVENT, INPUT_RECORD,
    KEY_EVENT, MOUSE_EVENT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};

use super::console::{ConsoleKeyRecord, ConsoleMouseRecord, ConsoleStreamDecoder};
use super::{DecodedEvents, HostInputDecoder};

const READ_BATCH: usize = 128;
const WIN32_INPUT_ENABLE: &[u8] = b"\x1b[?9001h";
const WIN32_INPUT_DISABLE: &[u8] = b"\x1b[?9001l";

/// Restore the console and host-terminal input modes when a client detaches.
pub struct WindowsInputModeGuard {
    input: Option<HANDLE>,
    restore_mode: Option<u32>,
    win32_mode: bool,
}

impl WindowsInputModeGuard {
    fn inactive() -> Self {
        Self {
            input: None,
            restore_mode: None,
            win32_mode: false,
        }
    }
}

impl Drop for WindowsInputModeGuard {
    fn drop(&mut self) {
        if self.win32_mode {
            let _ = std::io::stdout().write_all(WIN32_INPUT_DISABLE);
            let _ = std::io::stdout().flush();
        }
        if let (Some(input), Some(mode)) = (self.input, self.restore_mode) {
            let _ = unsafe { SetConsoleMode(input, mode) };
        }
    }
}

/// Enable lossless VT input and the documented Win32 key-record protocol.
///
/// Crossterm raw mode does not set `ENABLE_VIRTUAL_TERMINAL_INPUT`. Without it,
/// ConPTY translates a terminal paste back into legacy records and destroys
/// the byte boundaries before Luvus can identify one bracketed paste.
pub fn enable_input_mode() -> WindowsInputModeGuard {
    if !native_backend_enabled() {
        return WindowsInputModeGuard::inactive();
    }
    let Ok(input) = std_handle(STD_INPUT_HANDLE) else {
        return WindowsInputModeGuard::inactive();
    };
    let mut original = 0;
    if unsafe { GetConsoleMode(input, &mut original) } == 0 {
        return WindowsInputModeGuard::inactive();
    }
    let desired = original | ENABLE_VIRTUAL_TERMINAL_INPUT;
    if desired != original && unsafe { SetConsoleMode(input, desired) } == 0 {
        return WindowsInputModeGuard::inactive();
    }
    let mut applied = 0;
    if unsafe { GetConsoleMode(input, &mut applied) } == 0
        || applied & ENABLE_VIRTUAL_TERMINAL_INPUT == 0
    {
        if desired != original {
            let _ = unsafe { SetConsoleMode(input, original) };
        }
        return WindowsInputModeGuard::inactive();
    }
    if std::io::stdout().write_all(WIN32_INPUT_ENABLE).is_err()
        || std::io::stdout().flush().is_err()
    {
        if desired != original {
            let _ = unsafe { SetConsoleMode(input, original) };
        }
        return WindowsInputModeGuard::inactive();
    }
    WindowsInputModeGuard {
        input: Some(input),
        restore_mode: (desired != original).then_some(original),
        win32_mode: true,
    }
}

pub fn run_input_loop(mut pending: Vec<Event>, mut emit: impl FnMut(Event) -> bool) {
    for event in pending.drain(..) {
        if !emit(event) {
            return;
        }
    }

    if native_backend_enabled() {
        if let Ok(mut input) = NativeConsoleInput::open() {
            if input.virtual_terminal_input {
                input.run(&mut emit);
                return;
            }
        }
    }
    run_crossterm_fallback(&mut emit);
}

fn native_backend_enabled() -> bool {
    std::env::var("LUVUS_WINDOWS_INPUT_BACKEND")
        .map(|value| !value.eq_ignore_ascii_case("crossterm"))
        .unwrap_or(true)
}

fn run_crossterm_fallback(emit: &mut impl FnMut(Event) -> bool) {
    let mut decoder = HostInputDecoder::default();
    loop {
        if let Some(timeout) = decoder.wait_timeout() {
            match poll(timeout) {
                Ok(false) => {
                    if !emit_decoded(decoder.flush_expired(), emit) {
                        return;
                    }
                    continue;
                }
                Ok(true) => {}
                Err(_) => return,
            }
        }
        let Ok(event) = read() else {
            return;
        };
        if !emit_decoded(decoder.push(event), emit) {
            return;
        }
    }
}

fn emit_decoded(decoded: DecodedEvents, emit: &mut impl FnMut(Event) -> bool) -> bool {
    let mut connected = true;
    decoded.for_each(|event| {
        if connected {
            trace_decoded_event(&event);
            connected = emit(event);
        }
    });
    connected
}

struct NativeConsoleInput {
    input: HANDLE,
    output: Option<HANDLE>,
    virtual_terminal_input: bool,
    decoder: ConsoleStreamDecoder,
    records: [INPUT_RECORD; READ_BATCH],
}

impl NativeConsoleInput {
    fn open() -> io::Result<Self> {
        let input = std_handle(STD_INPUT_HANDLE)?;
        let mut mode = 0;
        if unsafe { GetConsoleMode(input, &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            input,
            output: std_handle(STD_OUTPUT_HANDLE).ok(),
            virtual_terminal_input: mode & ENABLE_VIRTUAL_TERMINAL_INPUT != 0,
            decoder: ConsoleStreamDecoder::default(),
            records: [INPUT_RECORD::default(); READ_BATCH],
        })
    }

    fn run(&mut self, emit: &mut impl FnMut(Event) -> bool) {
        loop {
            let now = Instant::now();
            let timeout = self.decoder.wait_timeout(now);
            match self.read_batch(timeout) {
                Ok(Some(read_count)) => {
                    let now = Instant::now();
                    for index in 0..read_count {
                        let decoded = self.decode_record(self.records[index], now);
                        if !emit_decoded(decoded, emit) {
                            return;
                        }
                    }
                }
                Ok(None) => {
                    if !emit_decoded(self.decoder.flush_expired(Instant::now()), emit) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    fn read_batch(&mut self, timeout: Option<Duration>) -> io::Result<Option<usize>> {
        let wait_ms = timeout
            .map(|timeout| u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1))
            .unwrap_or(INFINITE);
        match unsafe { WaitForSingleObject(self.input, wait_ms) } {
            WAIT_TIMEOUT => return Ok(None),
            WAIT_OBJECT_0 => {}
            WAIT_FAILED => return Err(io::Error::last_os_error()),
            _ => return Err(io::Error::other("unexpected console wait result")),
        }
        let mut read_count = 0;
        if unsafe {
            ReadConsoleInputW(
                self.input,
                self.records.as_mut_ptr(),
                self.records.len() as u32,
                &mut read_count,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(read_count as usize))
    }

    fn decode_record(&mut self, record: INPUT_RECORD, now: Instant) -> DecodedEvents {
        match u32::from(record.EventType) {
            KEY_EVENT => {
                let key = unsafe { record.Event.KeyEvent };
                let key = ConsoleKeyRecord {
                    key_down: key.bKeyDown != 0,
                    repeat_count: key.wRepeatCount,
                    virtual_key: key.wVirtualKeyCode,
                    scan_code: key.wVirtualScanCode,
                    utf16: unsafe { key.uChar.UnicodeChar },
                    control_state: key.dwControlKeyState,
                };
                trace_key_record(key);
                self.decoder.push_key(key, now)
            }
            MOUSE_EVENT => {
                let mouse = unsafe { record.Event.MouseEvent };
                let (column, row) =
                    self.relative_mouse_position(mouse.dwMousePosition.X, mouse.dwMousePosition.Y);
                self.decoder.push_mouse(
                    ConsoleMouseRecord {
                        column,
                        row,
                        button_state: mouse.dwButtonState,
                        control_state: mouse.dwControlKeyState,
                        event_flags: mouse.dwEventFlags,
                    },
                    now,
                )
            }
            WINDOW_BUFFER_SIZE_EVENT => crossterm::terminal::size()
                .map(|(columns, rows)| self.decoder.push_event(Event::Resize(columns, rows), now))
                .unwrap_or(DecodedEvents::None),
            FOCUS_EVENT => {
                let focus = unsafe { record.Event.FocusEvent };
                self.decoder.push_event(
                    if focus.bSetFocus != 0 {
                        Event::FocusGained
                    } else {
                        Event::FocusLost
                    },
                    now,
                )
            }
            _ => DecodedEvents::None,
        }
    }

    fn relative_mouse_position(&self, column: i16, row: i16) -> (u16, u16) {
        let fallback = (column.max(0) as u16, row.max(0) as u16);
        let Some(output) = self.output else {
            return fallback;
        };
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleScreenBufferInfo(output, &mut info) } == 0 {
            return fallback;
        }
        (
            column.saturating_sub(info.srWindow.Left).max(0) as u16,
            row.saturating_sub(info.srWindow.Top).max(0) as u16,
        )
    }
}

fn input_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("LUVUS_WINDOWS_INPUT_TRACE")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes"
                )
            })
            .unwrap_or(false)
    })
}

/// Classifies a UTF-16 unit without retaining the character itself.
///
/// 0 = none, 1 = control, 2 = printable, 3 = surrogate half.
fn utf16_class(unit: u16) -> u8 {
    match unit {
        0 => 0,
        0x01..=0x1f | 0x7f => 1,
        0xd800..=0xdfff => 3,
        _ => 2,
    }
}

// This diagnostic is deliberately opt-in and records only the UTF-16 unit
// class, never the character itself. It is never emitted during normal logging.
fn trace_key_record(record: ConsoleKeyRecord) {
    if !input_trace_enabled() {
        return;
    }
    crate::logging::event(
        crate::logging::EventKind::ClientInputRecord,
        &[
            crate::logging::Field::KeyDown(record.key_down),
            crate::logging::Field::RepeatCount(u64::from(record.repeat_count)),
            crate::logging::Field::VirtualKey(u64::from(record.virtual_key)),
            crate::logging::Field::ScanCode(u64::from(record.scan_code)),
            crate::logging::Field::Utf16Class(u64::from(utf16_class(record.utf16))),
            crate::logging::Field::ControlState(u64::from(record.control_state)),
        ],
    );
}

fn trace_decoded_event(event: &Event) {
    if !input_trace_enabled() {
        return;
    }
    let (kind, bytes) = match event {
        Event::Paste(text) => ("paste", text.len() as u64),
        Event::Key(_) => ("key", 0),
        Event::Mouse(_) => ("mouse", 0),
        Event::Resize(_, _) => ("resize", 0),
        Event::FocusGained => ("focus_gained", 0),
        Event::FocusLost => ("focus_lost", 0),
    };
    crate::logging::event(
        crate::logging::EventKind::ClientInputDecoded,
        &[
            crate::logging::Field::InputKind(
                crate::logging::SafeId::new(kind).expect("static input kind is safe"),
            ),
            crate::logging::Field::Bytes(bytes),
        ],
    );
}

fn std_handle(kind: u32) -> io::Result<HANDLE> {
    let handle = unsafe { GetStdHandle(kind) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows console handle unavailable",
        ))
    } else {
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_trace_classification_does_not_retain_characters() {
        assert_eq!(utf16_class(0), 0);
        assert_eq!(utf16_class(b'\n' as u16), 1);
        assert_eq!(utf16_class(b'a' as u16), 2);
        assert_eq!(utf16_class(0xd83d), 3);
    }

    #[test]
    fn virtual_terminal_input_preserves_existing_console_flags() {
        let original = 0x0010 | 0x0080;
        assert_eq!(
            original | ENABLE_VIRTUAL_TERMINAL_INPUT,
            0x0010 | 0x0080 | 0x0200
        );
    }
}
