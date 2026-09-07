//! Native Windows console input reader.
//!
//! Crossterm intentionally presents one semantic event at a time. Windows
//! Terminal, however, injects a bracketed paste as a burst of console records.
//! Reading those records directly preserves scan codes, UTF-16, modifiers,
//! repeats, mouse state, and paste boundaries before they cross Luvus IPC.

use std::io;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{poll, read, Event};
use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleInputW,
    CONSOLE_SCREEN_BUFFER_INFO, FOCUS_EVENT, INPUT_RECORD, KEY_EVENT, MOUSE_EVENT,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};

use super::console::{ConsoleInputDecoder, ConsoleKeyRecord, ConsoleMouseRecord};
use super::{DecodedEvents, HostInputDecoder};

const READ_BATCH: usize = 128;

pub fn run_input_loop(mut pending: Vec<Event>, mut emit: impl FnMut(Event) -> bool) {
    for event in pending.drain(..) {
        // Theme probing runs before bracketed paste is enabled, so these are
        // ordinary user events rather than paste markers. Forward them exactly
        // once before the native reader begins consuming console records.
        if !emit(event) {
            return;
        }
    }

    if native_backend_enabled() {
        if let Ok(mut input) = NativeConsoleInput::open() {
            input.run(&mut emit);
            return;
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
            connected = emit(event);
        }
    });
    connected
}

struct NativeConsoleInput {
    input: HANDLE,
    output: Option<HANDLE>,
    decoder: ConsoleInputDecoder,
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
            decoder: ConsoleInputDecoder::default(),
            records: [INPUT_RECORD::default(); READ_BATCH],
        })
    }

    fn run(&mut self, emit: &mut impl FnMut(Event) -> bool) {
        loop {
            let timeout = self.decoder.wait_timeout();
            match self.read_batch(timeout) {
                Ok(Some(read_count)) => {
                    let now = Instant::now();
                    for index in 0..read_count {
                        let record = self.records[index];
                        let decoded = self.decode_record(record, now);
                        if !emit_decoded(decoded, emit) {
                            return;
                        }
                    }
                }
                Ok(None) => {
                    if !emit_decoded(self.decoder.flush_expired(), emit) {
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
                self.decoder.push_key(
                    ConsoleKeyRecord {
                        key_down: key.bKeyDown != 0,
                        repeat_count: key.wRepeatCount,
                        virtual_key: key.wVirtualKeyCode,
                        scan_code: key.wVirtualScanCode,
                        utf16: unsafe { key.uChar.UnicodeChar },
                        control_state: key.dwControlKeyState,
                    },
                    now,
                )
            }
            MOUSE_EVENT => {
                let mouse = unsafe { record.Event.MouseEvent };
                let row = self.relative_mouse_row(mouse.dwMousePosition.Y);
                self.decoder.push_mouse(
                    ConsoleMouseRecord {
                        column: mouse.dwMousePosition.X.max(0) as u16,
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
                let event = if focus.bSetFocus != 0 {
                    Event::FocusGained
                } else {
                    Event::FocusLost
                };
                self.decoder.push_event(event, now)
            }
            _ => DecodedEvents::None,
        }
    }

    fn relative_mouse_row(&self, absolute: i16) -> u16 {
        let Some(output) = self.output else {
            return absolute.max(0) as u16;
        };
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleScreenBufferInfo(output, &mut info) } == 0 {
            return absolute.max(0) as u16;
        }
        absolute.saturating_sub(info.srWindow.Top).max(0) as u16
    }
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
