//! "Copy code" on Add contact (design decision 2026-11-05): this PC's code goes on
//! the clipboard marked to stay out of clipboard monitors, Win+V history and
//! cloud clipboard, and is cleared 60 s later, or when MeshLive exits, but
//! only while the clipboard still holds that copy (a later copy of anything
//! else is never erased).
use std::time::{Duration, Instant};
use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE, HWND},
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData},
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::CF_UNICODETEXT,
        },
    },
    core::w,
};

pub const CLEAR_AFTER: Duration = Duration::from_secs(60);
/// Another program held the clipboard when the copy was due to go: try again
/// this soon (the code must not stay just because the clipboard was busy once).
pub const RETRY_AFTER: Duration = Duration::from_secs(1);

/// A copy that MeshLive will clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingClear {
    pub due: Instant,
    /// The clipboard sequence number right after the copy.
    pub sequence: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not yet: wake up at this time.
    Wait(Instant),
    /// The copy is still there and its time is up.
    Clear,
    /// Something else was copied since: leave the clipboard alone.
    Leave,
}

impl PendingClear {
    pub fn new(copied_at: Instant, sequence: u32) -> Self {
        Self { due: copied_at + CLEAR_AFTER, sequence }
    }
    pub fn decide(&self, now: Instant, current: u32) -> Decision {
        if current != self.sequence {
            Decision::Leave
        } else if now >= self.due {
            Decision::Clear
        } else {
            Decision::Wait(self.due)
        }
    }
    /// The clipboard was busy at the due time: the same copy, due again soon.
    pub fn retry_later(&self, now: Instant) -> Self {
        Self { due: now + RETRY_AFTER, sequence: self.sequence }
    }
    /// MeshLive is closing: the code goes now if it is still there.
    pub fn decide_at_exit(&self, current: u32) -> Decision {
        if current == self.sequence { Decision::Clear } else { Decision::Leave }
    }
}

pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

/// Opens the clipboard, retrying briefly while another program holds it.
fn open(owner: Option<HWND>) -> bool {
    for attempt in 0..5 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return true;
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    false
}

/// One clipboard format from bytes; on success the system owns the memory.
unsafe fn put(format: u32, bytes: &[u8]) -> windows::core::Result<()> {
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }?;
    let target = unsafe { GlobalLock(memory) };
    if target.is_null() {
        let error = windows::core::Error::from_thread();
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(error);
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast::<u8>(), bytes.len()) };
    // Returns "failure" with no error once the lock count reaches zero.
    let _ = unsafe { GlobalUnlock(memory) };
    if let Err(error) = unsafe { SetClipboardData(format, Some(HANDLE(memory.0))) } {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(error);
    }
    Ok(())
}

/// Puts `text` on the clipboard; returns the sequence number to clear later.
pub fn copy_secret(owner: Option<HWND>, text: &str) -> Option<u32> {
    if !open(owner) {
        return None;
    }
    let result = (|| -> windows::core::Result<()> {
        unsafe { EmptyClipboard() }?;
        let utf16: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        unsafe { put(u32::from(CF_UNICODETEXT.0), &utf16) }?;
        for name in [w!("ExcludeClipboardContentFromMonitorProcessing"), w!("CanIncludeInClipboardHistory"), w!("CanUploadToCloudClipboard")] {
            let format = unsafe { RegisterClipboardFormatW(name) };
            if format != 0 {
                unsafe { put(format, &0u32.to_le_bytes()) }?;
            }
        }
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result.ok()?;
    Some(sequence())
}

/// What a clear attempt did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleared {
    /// The copy was removed.
    Yes,
    /// Something else was copied since: left alone.
    Replaced,
    /// Another program held the clipboard: nothing changed, try again.
    Busy,
}

/// Empties the clipboard if it still holds the copy `pending` made.
pub fn clear_if_unchanged(pending: PendingClear) -> Cleared {
    if !open(None) {
        return Cleared::Busy;
    }
    // Checked again with the clipboard open, so nothing can land in between.
    let cleared = if sequence() != pending.sequence {
        Cleared::Replaced
    } else if unsafe { EmptyClipboard() }.is_ok() {
        Cleared::Yes
    } else {
        Cleared::Busy
    };
    let _ = unsafe { CloseClipboard() };
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the decisions are tested: a test must never touch the clipboard of
    // the person running it.

    #[test]
    fn a_copied_code_stays_for_60_seconds_then_is_cleared() {
        let at = Instant::now();
        let pending = PendingClear::new(at, 41);
        assert_eq!(pending.decide(at, 41), Decision::Wait(at + CLEAR_AFTER));
        assert_eq!(pending.decide(at + Duration::from_secs(59), 41), Decision::Wait(at + CLEAR_AFTER));
        assert_eq!(pending.decide(at + CLEAR_AFTER, 41), Decision::Clear);
    }

    #[test]
    fn something_copied_later_is_never_cleared() {
        let at = Instant::now();
        let pending = PendingClear::new(at, 41);
        assert_eq!(pending.decide(at + Duration::from_secs(5), 42), Decision::Leave);
        assert_eq!(pending.decide(at + CLEAR_AFTER, 42), Decision::Leave);
    }

    #[test]
    fn a_busy_clipboard_at_the_due_time_is_tried_again_soon() {
        let at = Instant::now();
        let due = PendingClear::new(at, 41);
        let busy_at = at + CLEAR_AFTER;
        let again = due.retry_later(busy_at);
        assert_eq!(again.decide(busy_at, 41), Decision::Wait(busy_at + RETRY_AFTER));
        assert_eq!(again.decide(busy_at + RETRY_AFTER, 41), Decision::Clear);
        assert_eq!(again.decide(busy_at + RETRY_AFTER, 42), Decision::Leave, "a later copy still wins");
    }

    #[test]
    fn closing_meshlive_clears_the_code_at_once_but_not_a_later_copy() {
        let pending = PendingClear::new(Instant::now(), 7);
        assert_eq!(pending.decide_at_exit(7), Decision::Clear);
        assert_eq!(pending.decide_at_exit(8), Decision::Leave);
    }
}
