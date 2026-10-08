//! Win32 glue between the Slint window and the WebView2 child windows.

use std::cell::RefCell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, LoadImageW, SendMessageW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, ICON_BIG, ICON_SMALL,
    IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNORMAL,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOVE, WM_MOVING, WM_RBUTTONDOWN, WM_SETICON, WM_SIZE, WS_CLIPCHILDREN,
};

thread_local! {
    /// Controller of the visible tab, so window moves can be forwarded without touching app state.
    static ACTIVE_CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
}

pub fn set_active_controller(controller: Option<ICoreWebView2Controller>) {
    ACTIVE_CONTROLLER.with(|c| *c.borrow_mut() = controller);
}

pub fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// Prepares the Slint top-level window for hosting WebView2 child windows.
pub fn prepare_main_window(hwnd: HWND) {
    unsafe {
        // Keep Slint's software renderer from painting over the WebView child windows.
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if style & WS_CLIPCHILDREN.0 as isize == 0 {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style | WS_CLIPCHILDREN.0 as isize);
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        }
        let _ = SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0);
    }
    set_window_icon(hwnd);
}

/// Points the window (title bar and Alt+Tab) at the icon embedded in the executable. Without this
/// the window shows Windows' default application icon even though the .exe itself has one.
fn set_window_icon(hwnd: HWND) {
    // MAKEINTRESOURCE(1): resource ids travel in the pointer's address, matching
    // `1 ICON "icon.ico"` in assets/litebrowser.rc. It is an id, never dereferenced.
    const ICON_RESOURCE: PCWSTR = PCWSTR(std::ptr::without_provenance(1));
    unsafe {
        let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(PCWSTR::null())
            .map(|h| h.into())
            .unwrap_or_default();
        for (which, size) in [(ICON_BIG, 32), (ICON_SMALL, 16)] {
            if let Ok(icon) =
                LoadImageW(Some(hinstance), ICON_RESOURCE, IMAGE_ICON, size, size, LR_DEFAULTSIZE | LR_SHARED)
            {
                SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}

/// Forces a full repaint of the window and its children.
pub fn repaint_all(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW};
    unsafe {
        let _ = RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW);
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        // WebView2 positions its own popups (<select> lists, context menus) relative to the
        // parent window, so it must hear about moves.
        WM_MOVE | WM_MOVING => {
            ACTIVE_CONTROLLER.with(|c| {
                if let Ok(c) = c.try_borrow() {
                    if let Some(controller) = c.as_ref() {
                        let _ = unsafe { controller.NotifyParentWindowPositionChanged() };
                    }
                }
            });
        }
        // Restoring or resizing can leave the software-rendered chrome stale next to the native
        // child window, so repaint everything.
        WM_SIZE => {
            const SIZE_MINIMIZED: usize = 1;
            if wparam.0 != SIZE_MINIMIZED {
                repaint_all(hwnd);
            }
        }
        // A click on the Slint UI while the page has keyboard focus: take the focus back,
        // otherwise typing into the address bar would go to the web page.
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => unsafe {
            if GetFocus() != hwnd {
                let _ = SetFocus(Some(hwnd));
            }
        },
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Moves keyboard focus from the web page back to the Slint window.
pub fn focus_main_window(hwnd: HWND) {
    unsafe {
        if GetFocus() != hwnd {
            let _ = SetFocus(Some(hwnd));
        }
    }
}

/// Opens a folder or file with its default application (Explorer for folders).
pub fn shell_open(path: &std::path::Path) {
    let target = HSTRING::from(path.as_os_str());
    unsafe {
        ShellExecuteW(None, &HSTRING::from("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}
