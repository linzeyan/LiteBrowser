//! Win32 glue between the Slint window and the WebView2 child windows.

use std::cell::{Cell, RefCell};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, GetKeyboardState, SetFocus, SetKeyboardState, VK_CAPITAL, VK_CONTROL, VK_MENU,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, IsChild, IsWindowVisible, LoadImageW, SendMessageW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
    ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, SC_KEYMENU, SWP_FRAMECHANGED, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNORMAL, WA_INACTIVE, WM_ACTIVATE, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOVE, WM_MOVING, WM_PAINT, WM_RBUTTONDOWN, WM_SETFOCUS,
    WM_SETICON, WM_SHOWWINDOW, WM_SIZE, WM_SYSCOMMAND, WS_CLIPCHILDREN,
};
use windows::Win32::Graphics::Gdi::GetUpdateRect;

use crate::logging::trace;

thread_local! {
    /// Controller of the visible tab, so window moves can be forwarded without touching app state.
    static ACTIVE_CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    /// The web page window that had keyboard focus when ours was deactivated.
    static PAGE_FOCUS: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
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
    // The messages behind a window that vanishes, won't come up, or loses its focus.
    let name = match msg {
        WM_ACTIVATE => "WM_ACTIVATE",
        WM_SETFOCUS => "WM_SETFOCUS",
        WM_KILLFOCUS => "WM_KILLFOCUS",
        WM_SHOWWINDOW => "WM_SHOWWINDOW",
        WM_SIZE => "WM_SIZE",
        WM_SYSCOMMAND => "WM_SYSCOMMAND",
        _ => "",
    };
    if !name.is_empty() {
        trace!("{name} wparam={:#x} lparam={:#x}", wparam.0, lparam.0);
    }
    // winit stamps a button press with the last position a WM_MOUSEMOVE gave it, but while a
    // native menu is open the moves go to the menu. The click that closes a tab's menu by landing
    // on another tab was taken as a press on the first tab, and the move that followed as a drag:
    // the two tabs swapped. Hand winit the click's own position first; a move to where the cursor
    // already is raises no event, so ordinary clicks are unaffected.
    if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
        unsafe { DefSubclassProc(hwnd, WM_MOUSEMOVE, wparam, lparam) };
    }
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
        // Windows wants a part of the window painted that something covered, usually a WebView
        // that just moved away. Slint only redraws what it changed itself, so whatever it drew
        // under that WebView never reached the screen: have it draw everything again. winit's
        // own redraw requests carry no update region, so this doesn't feed on itself.
        WM_PAINT => unsafe {
            if GetUpdateRect(hwnd, None, false).as_bool() {
                crate::app::post(crate::app::Event::RepaintAll);
            }
        },
        // Restoring or resizing can leave the software-rendered chrome stale next to the native
        // child window, so repaint everything.
        WM_SIZE => {
            const SIZE_MINIMIZED: usize = 1;
            if wparam.0 != SIZE_MINIMIZED {
                repaint_all(hwnd);
            }
        }
        // A click on the Slint UI while the page has keyboard focus: take the focus back,
        // otherwise typing into the address bar would go to the web page. Only a left click: the
        // focus would land in the address bar, so a tab's or bookmark's menu (right) or closing
        // a tab (middle) would leave the page unable to type into. Like Chrome, those keep the
        // focus where it is; the address bar's own menu takes it itself.
        WM_LBUTTONDOWN => unsafe {
            PAGE_FOCUS.set(HWND::default());
            if GetFocus() != hwnd {
                let _ = SetFocus(Some(hwnd));
            }
        },
        // Activation gives focus to the top-level window, so coming back from another app left
        // the page unable to type into. Like Chrome, remember the page window that had focus on
        // the way out and hand it back on the way in, after the default handling.
        WM_ACTIVATE => unsafe {
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                let focus = GetFocus();
                PAGE_FOCUS.set(if IsChild(hwnd, focus).as_bool() { focus } else { HWND::default() });
            } else {
                let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                restore_page_focus(hwnd);
                return result;
            }
        },
        // Slint's editing shortcuts only match lower-case letters, and with Caps Lock on winit
        // reports Ctrl+V as "V", so copy/paste did nothing in every text field (with 注音, Caps
        // Lock is how English gets typed). Hide the toggle while winit reads the key.
        WM_KEYDOWN | WM_KEYUP => unsafe {
            let down = |vk: u16| GetKeyState(vk as i32) < 0;
            if down(VK_CONTROL.0) && !down(VK_MENU.0) && GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0 {
                let mut keys = [0u8; 256];
                if GetKeyboardState(&mut keys).is_ok() {
                    keys[VK_CAPITAL.0 as usize] &= !1;
                    let _ = SetKeyboardState(&keys);
                    let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                    // Re-read: handling the key may have moved the rest of the state on.
                    if GetKeyboardState(&mut keys).is_ok() {
                        keys[VK_CAPITAL.0 as usize] |= 1;
                        let _ = SetKeyboardState(&keys);
                    }
                    return result;
                }
            }
        },
        // A lone Alt (or F10) puts the window into the menu bar's keyboard mode. With no menu bar
        // nothing shows, and the next click only ends the mode: a tab could not be clicked or
        // dragged. Chrome never enters it. Alt+Space (lparam ' ') still opens the system menu.
        WM_SYSCOMMAND if (wparam.0 & 0xFFF0) as u32 == SC_KEYMENU && lparam.0 == 0 => return LRESULT(0),
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// What decides whether Windows lists the window (taskbar, Alt+Tab, Task Manager's Apps): shown,
/// minimized, cloaked, owned, tool window, on a monitor. Logged when it changes, so a window that
/// "disappears" leaves a trace.
pub fn window_state(hwnd: HWND) -> String {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindow, GetWindowRect, IsIconic, IsZoomed, GWL_EXSTYLE, GW_OWNER,
    };
    unsafe {
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let owner = GetWindow(hwnd, GW_OWNER).map_or(std::ptr::null_mut(), |h| h.0);
        format!(
            "visible={} minimized={} maximized={} cloaked={cloaked} on_monitor={} owner={owner:?} style={:#x} exstyle={:#x} rect=({},{})-({},{})",
            IsWindowVisible(hwnd).as_bool() as u8,
            IsIconic(hwnd).as_bool() as u8,
            IsZoomed(hwnd).as_bool() as u8,
            !MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL).is_invalid() as u8,
            GetWindowLongPtrW(hwnd, GWL_STYLE),
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE),
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
        )
    }
}

/// Restores the window if it is minimized and puts it in front. Only works while we may take the
/// foreground, e.g. right after a second launch allowed it. Returns whether Windows let us.
pub fn bring_to_front(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE};
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// Moves keyboard focus from the web page back to the Slint window.
pub fn focus_main_window(hwnd: HWND) {
    PAGE_FOCUS.set(HWND::default());
    unsafe {
        if GetFocus() != hwnd {
            let _ = SetFocus(Some(hwnd));
        }
    }
}

/// Gives focus back to the page window that had it when the window was deactivated, if focus is
/// now on the window itself and that page is still alive and shown (the same tab). Also called
/// after restoring from minimized, because the page is hidden while minimized and only comes
/// back after activation.
pub fn restore_page_focus(hwnd: HWND) {
    unsafe {
        let page = PAGE_FOCUS.get();
        if GetFocus() == hwnd && IsChild(hwnd, page).as_bool() && IsWindowVisible(page).as_bool() {
            let _ = SetFocus(Some(page));
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
