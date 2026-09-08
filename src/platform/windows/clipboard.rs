//! Explicit, bounded Windows clipboard-image access.

use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

use crate::clipboard_image::{dib_to_png, validated_png_prefix, MAX_PNG_BYTES};

const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const MAX_OPEN_ATTEMPTS: usize = 3;

struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Option<Self> {
        for attempt in 0..MAX_OPEN_ATTEMPTS {
            // SAFETY: a null owner is documented for read-only clipboard use.
            if unsafe { OpenClipboard(std::ptr::null_mut::<core::ffi::c_void>() as HWND) } != 0 {
                return Some(Self);
            }
            if attempt + 1 < MAX_OPEN_ATTEMPTS {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        None
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: this guard exists only after one successful OpenClipboard.
        unsafe {
            CloseClipboard();
        }
    }
}

struct GlobalLockGuard(HANDLE);

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        // SAFETY: this handle was locked successfully and remains owned by the
        // clipboard. Unlocking does not free or transfer it.
        unsafe {
            GlobalUnlock(self.0);
        }
    }
}

pub(super) fn clipboard_image() -> Option<Vec<u8>> {
    let _clipboard = ClipboardGuard::open()?;
    let png_name = ['P' as u16, 'N' as u16, 'G' as u16, 0];
    // SAFETY: `png_name` is a live null-terminated UTF-16 string.
    let png_format = unsafe { RegisterClipboardFormatW(png_name.as_ptr()) };
    if png_format != 0 {
        if let Some(png) = with_format(png_format, |bytes| {
            validated_png_prefix(bytes).ok().map(|png| png.to_vec())
        })
        .flatten()
        {
            return Some(png);
        }
    }
    for format in [CF_DIBV5, CF_DIB] {
        if let Some(png) = with_format(format, dib_to_png).and_then(Result::ok) {
            return Some(png);
        }
    }
    None
}

fn with_format<T>(format: u32, read: impl FnOnce(&[u8]) -> T) -> Option<T> {
    // SAFETY: querying an integer clipboard format has no pointer preconditions.
    if unsafe { IsClipboardFormatAvailable(format) } == 0 {
        return None;
    }
    // SAFETY: the clipboard is open for this thread and `format` was reported
    // available. Luvus never takes ownership of the returned handle.
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: clipboard memory for PNG and DIB formats is an HGLOBAL. Size is
    // queried before lock and bounded before constructing the borrowed slice.
    let size = unsafe { GlobalSize(handle) };
    if size == 0 || size > MAX_PNG_BYTES {
        return None;
    }
    // SAFETY: a successful GlobalLock returns at least `size` readable bytes
    // which remain valid until the matching unlock below.
    let pointer = unsafe { GlobalLock(handle) };
    if pointer.is_null() {
        return None;
    }
    let _lock = GlobalLockGuard(handle);
    // SAFETY: established by GlobalSize and GlobalLock above. The clipboard is
    // held open and the handle stays locked for the closure call.
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) };
    Some(read(bytes))
}
