//! The notification-area (tray) icon: while Allow calls is on, closing the
//! window hides it here so phones can still call this PC. A message-only
//! window on its own thread owns the icon (winit owns the main window's
//! messages); its menu is Open MeshLive and Exit. Clicking the icon opens
//! the window. Events reach the app through `on_event`.
use std::sync::Mutex;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW},
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

/// What the tray asks the app to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    Exit,
}

/// What the window's close button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    /// Allow calls is on: keep running in the notification area.
    HideToTray,
    Exit,
}

/// Closing must not stop calls: while calls are allowed the window hides to
/// the tray; otherwise closing quits (pure).
pub fn close_action(calls_allowed: bool) -> CloseAction {
    if calls_allowed { CloseAction::HideToTray } else { CloseAction::Exit }
}

/// The first-time balloon.
pub const HINT_TITLE: &str = "MeshLive is still running";
pub const HINT_TEXT: &str = "MeshLive keeps running so phones can call you. Right-click the icon to exit.";

const CALLBACK: u32 = WM_APP + 1;
const BALLOON: u32 = WM_APP + 2;
const MENU_OPEN: usize = 1;
const MENU_EXIT: usize = 2;

type Handler = Box<dyn Fn(TrayEvent) + Send>;
static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);
static PENDING_BALLOON: Mutex<Option<(String, String)>> = Mutex::new(None);

pub struct Tray {
    window: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn wide(text: &str, out: &mut [u16]) {
    for (slot, unit) in out.iter_mut().zip(text.encode_utf16().chain(std::iter::once(0))) {
        *slot = unit;
    }
    if let Some(last) = out.last_mut() {
        *last = 0;
    }
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: window, uID: 1, ..Default::default() };
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = CALLBACK;
    // SAFETY: plain Win32 resource loads; the app icon is resource 1 (meshlive.rc).
    data.hIcon = unsafe {
        let module = GetModuleHandleW(None).ok();
        let size = GetSystemMetrics(SM_CXSMICON);
        module
            .and_then(|m| LoadImageW(Some(m.into()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR).ok())
            .map(|h| HICON(h.0))
            .or_else(|| LoadIconW(None, IDI_APPLICATION).ok())
            .unwrap_or_default()
    };
    wide("MeshLive", &mut data.szTip);
    data
}

fn send(event: TrayEvent) {
    if let Some(handler) = HANDLER.lock().unwrap().as_ref() {
        handler(event);
    }
}

extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: called by Windows on the tray thread with a valid window.
    unsafe {
        match message {
            CALLBACK => {
                match lparam.0 as u32 {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => send(TrayEvent::Open),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        if let Ok(menu) = CreatePopupMenu() {
                            let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Open MeshLive"));
                            let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("Exit"));
                            let mut at = POINT::default();
                            let _ = GetCursorPos(&mut at);
                            // The menu closes when the user clicks elsewhere
                            // only if this window is in the foreground.
                            let _ = SetForegroundWindow(window);
                            let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, None, window, None);
                            let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
                            let _ = DestroyMenu(menu);
                            match chosen.0 as usize {
                                MENU_OPEN => send(TrayEvent::Open),
                                MENU_EXIT => send(TrayEvent::Exit),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            BALLOON => {
                if let Some((title, text)) = PENDING_BALLOON.lock().unwrap().take() {
                    let mut data = icon_data(window);
                    data.uFlags = NIF_INFO;
                    data.dwInfoFlags = NIIF_INFO;
                    wide(&title, &mut data.szInfoTitle);
                    wide(&text, &mut data.szInfo);
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let data = icon_data(window);
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                let _ = DestroyWindow(window);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => {
                // Explorer restarted: the icon must be added again.
                if message == taskbar_created() {
                    let data = icon_data(window);
                    let _ = Shell_NotifyIconW(NIM_ADD, &data);
                    return LRESULT(0);
                }
                DefWindowProcW(window, message, wparam, lparam)
            }
        }
    }
}

fn taskbar_created() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: a plain registered-message lookup.
    *ID.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

impl Tray {
    /// Adds the icon. `on_event` runs on the tray thread.
    pub fn start(on_event: impl Fn(TrayEvent) + Send + 'static) -> Option<Self> {
        *HANDLER.lock().unwrap() = Some(Box::new(on_event));
        let (ready, wait) = std::sync::mpsc::channel::<isize>();
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                // SAFETY: a message-only window owned by this thread, and its loop.
                unsafe {
                    let class = w!("MeshLive.Tray");
                    let instance = GetModuleHandleW(None).unwrap_or_default();
                    let wc = WNDCLASSW { lpfnWndProc: Some(procedure), hInstance: instance.into(), lpszClassName: class, ..Default::default() };
                    RegisterClassW(&wc);
                    let window = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("MeshLive tray"), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(instance.into()), None);
                    let Ok(window) = window else {
                        let _ = ready.send(0);
                        return;
                    };
                    let _ = taskbar_created();
                    let data = icon_data(window);
                    if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
                        let _ = DestroyWindow(window);
                        let _ = ready.send(0);
                        return;
                    }
                    let _ = ready.send(window.0 as isize);
                    let mut message = MSG::default();
                    while GetMessageW(&mut message, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            })
            .ok()?;
        let window = wait.recv().ok().filter(|w| *w != 0)?;
        Some(Self { window, thread: Some(thread) })
    }
    /// A one-line notice from the icon (shown once by the caller).
    pub fn balloon(&self, title: &str, text: &str) {
        *PENDING_BALLOON.lock().unwrap() = Some((title.to_owned(), text.to_owned()));
        // SAFETY: posting to this tray's own window.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.window as _)), BALLOON, WPARAM(0), LPARAM(0));
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: posting to this tray's own window; its thread removes the icon.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.window as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        *HANDLER.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_hides_to_the_tray_only_while_calls_are_allowed() {
        assert_eq!(close_action(true), CloseAction::HideToTray);
        assert_eq!(close_action(false), CloseAction::Exit);
    }

    #[test]
    fn tray_text_fits_its_fixed_buffers() {
        let data = NOTIFYICONDATAW::default();
        assert!(HINT_TITLE.encode_utf16().count() < data.szInfoTitle.len());
        assert!(HINT_TEXT.encode_utf16().count() < data.szInfo.len());
    }
}
