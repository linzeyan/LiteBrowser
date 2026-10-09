//! Process- and OS-level Windows helpers: DPAPI, system memory, single instance, app identity,
//! native popup menus, and the file-open dialog. (Window/HWND glue lives in `win`.)

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, FindWindowW, GetCursorPos, GetMenuItemCount, SendMessageW,
    SetForegroundWindow, SetMenuItemInfoW, TrackPopupMenuEx, HMENU, MENUITEMINFOW, MF_CHECKED, MF_DISABLED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_BITMAP, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON,
    TPM_TOPALIGN, TRACK_POPUP_MENU_FLAGS, WM_COPYDATA,
};

/// A hidden window class/title used to find a running instance. Also the WM_COPYDATA tag.
const SINGLETON_CLASS: PCWSTR = windows::core::w!("LiteBrowser_Singleton");
pub const WM_COPYDATA_OPEN_URL: usize = 0x4C42; // 'LB'

/// Decrypts a DPAPI blob (CryptUnprotectData) for the current user. Used only for LiteBrowser's
/// own WebView2 key when writing imported passwords into our own store.
pub fn dpapi_unprotect(blob: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output).ok()?;
        if output.pbData.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(output.pbData as *mut _)));
        Some(data)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMemory {
    pub free_bytes: u64,
}

pub fn system_memory() -> SystemMemory {
    unsafe {
        let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if GlobalMemoryStatusEx(&mut status).is_ok() {
            SystemMemory { free_bytes: status.ullAvailPhys }
        } else {
            SystemMemory::default()
        }
    }
}

/// Sets an explicit AppUserModelID so Windows groups our windows under one taskbar button
/// and uses our icon (rather than lumping us in with the WebView2 host).
pub fn set_app_id() {
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from("LiteBrowser.Browser"));
    }
}

/// Holds the single-instance mutex for the process lifetime.
pub struct SingleInstance {
    _handle: HANDLE,
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self._handle);
        }
    }
}

/// Returns `Ok(guard)` when this is the first instance. When another instance is already running,
/// returns `Err(())` after handing it `url_to_open` (so clicking a link opens a tab there).
pub fn acquire_single_instance(url_to_open: Option<&str>) -> Result<SingleInstance, ()> {
    unsafe {
        let name = HSTRING::from("Local\\LiteBrowser_SingleInstance");
        let handle = match CreateMutexW(None, true, &name) {
            Ok(h) => h,
            Err(_) => return Err(()), // treat as "cannot be sure" → let caller decide (it starts normally)
        };
        let already = windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS;
        if already {
            let _ = CloseHandle(handle);
            send_to_existing(url_to_open);
            return Err(());
        }
        Ok(SingleInstance { _handle: handle })
    }
}

fn send_to_existing(url: Option<&str>) {
    unsafe {
        let Ok(hwnd) = FindWindowW(SINGLETON_CLASS, PCWSTR::null()) else { return };
        if hwnd.0.is_null() {
            return;
        }
        let _ = SetForegroundWindow(hwnd);
        if let Some(url) = url.filter(|u| !u.is_empty()) {
            let bytes = url.as_bytes();
            let cds = COPYDATASTRUCT {
                dwData: WM_COPYDATA_OPEN_URL,
                cbData: bytes.len() as u32,
                lpData: bytes.as_ptr() as *mut _,
            };
            SendMessageW(
                hwnd,
                WM_COPYDATA,
                Some(WPARAM(0)),
                Some(LPARAM(&cds as *const _ as isize)),
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Native popup menu

/// A menu item's icon: `size`×`size` premultiplied RGBA, already at the menu's pixel size.
#[derive(Clone, Debug)]
pub struct MenuIcon {
    pub size: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum MenuItem {
    Entry { id: u32, label: String, checked: bool, enabled: bool, icon: Option<MenuIcon> },
    Separator,
    Submenu { label: String, items: Vec<MenuItem>, icon: Option<MenuIcon> },
}

impl MenuItem {
    pub fn entry(id: u32, label: impl Into<String>) -> Self {
        MenuItem::Entry { id, label: label.into(), checked: false, enabled: true, icon: None }
    }
    pub fn disabled(label: impl Into<String>) -> Self {
        MenuItem::Entry { id: 0, label: label.into(), checked: false, enabled: false, icon: None }
    }
    pub fn with_icon(mut self, new: Option<MenuIcon>) -> Self {
        if let MenuItem::Entry { icon, .. } | MenuItem::Submenu { icon, .. } = &mut self {
            *icon = new;
        }
        self
    }
}

thread_local! {
    static OWNER: RefCell<Option<HWND>> = const { RefCell::new(None) };
}

/// Remembers the owner window used for popup menus (so they dismiss correctly).
pub fn set_menu_owner(hwnd: HWND) {
    OWNER.with(|o| *o.borrow_mut() = Some(hwnd));
}

/// Shows a blocking popup menu at the cursor and returns the chosen item id (0 = cancelled).
pub fn popup_menu(items: &[MenuItem]) -> u32 {
    let mut pt = POINT::default();
    let _ = unsafe { GetCursorPos(&mut pt) };
    track_menu(items, pt, TPM_RETURNCMD | TPM_RIGHTBUTTON)
}

/// Like `popup_menu`, but hanging from (x, y) of the owner's client area, in physical pixels: by
/// its top-right corner for Chrome's main menu below the ⋮ button, by its top-left corner for a
/// bookmarks-bar folder.
pub fn popup_menu_below(items: &[MenuItem], x: i32, y: i32, right_aligned: bool) -> u32 {
    let Some(owner) = OWNER.with(|o| *o.borrow()) else { return 0 };
    let mut pt = POINT { x, y };
    let _ = unsafe { ClientToScreen(owner, &mut pt) };
    let align = if right_aligned { TPM_RIGHTALIGN } else { TPM_LEFTALIGN };
    track_menu(items, pt, TPM_RETURNCMD | TPM_RIGHTBUTTON | align | TPM_TOPALIGN)
}

fn track_menu(items: &[MenuItem], pt: POINT, flags: TRACK_POPUP_MENU_FLAGS) -> u32 {
    let owner = OWNER.with(|o| *o.borrow());
    let Some(owner) = owner else { return 0 };
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        // A menu does not own its items' bitmaps; they are freed once it is gone.
        let mut bitmaps = Vec::new();
        for item in items {
            append(menu, item, &mut bitmaps);
        }
        let _ = SetForegroundWindow(owner);
        let chosen = TrackPopupMenuEx(menu, flags.0, pt.x, pt.y, owner, None);
        let _ = DestroyMenu(menu);
        for bitmap in bitmaps {
            let _ = DeleteObject(bitmap.into());
        }
        chosen.0 as u32
    }
}

// ---------------------------------------------------------------------------------------------
// File-open dialog (password CSV)

/// Shows a "choose a CSV" dialog and returns the picked path.
pub fn open_csv_dialog(owner: HWND) -> Option<PathBuf> {
    use windows::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OPENFILENAMEW};
    unsafe {
        // Double-null-terminated filter: "label\0pattern\0...\0\0".
        let mut filter: Vec<u16> = "CSV 檔 (*.csv)\0*.csv\0所有檔案 (*.*)\0*.*\0\0".encode_utf16().collect();
        let mut buffer = [0u16; 1024];
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: owner,
            lpstrFilter: PCWSTR(filter.as_mut_ptr()),
            lpstrFile: windows::core::PWSTR(buffer.as_mut_ptr()),
            nMaxFile: buffer.len() as u32,
            Flags: OFN_FILEMUSTEXIST | OFN_HIDEREADONLY,
            ..Default::default()
        };
        if GetOpenFileNameW(&mut ofn).as_bool() {
            let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            Some(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Hidden singleton window: receives WM_COPYDATA from a second launch to open a URL here.

/// Callback run when another instance forwards a URL to this one.
type OpenUrlCallback = Box<dyn Fn(String)>;

thread_local! {
    static ON_OPEN_URL: RefCell<Option<OpenUrlCallback>> = const { RefCell::new(None) };
    static SINGLETON_HWND: RefCell<Option<HWND>> = const { RefCell::new(None) };
}

/// Creates the hidden window a second launch finds via `FindWindowW`. `on_url` runs on the UI
/// thread when another instance forwards a URL.
pub fn create_singleton_window(on_url: impl Fn(String) + 'static) {
    ON_OPEN_URL.with(|c| *c.borrow_mut() = Some(Box::new(on_url)));
    unsafe {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, RegisterClassW, HMENU, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
        };
        let hinstance = GetModuleHandleW(PCWSTR::null()).map(|h| h.into()).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(singleton_proc),
            hInstance: hinstance,
            lpszClassName: SINGLETON_CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        // A hidden (never shown) top-level window, so FindWindowW can locate it by class.
        if let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            SINGLETON_CLASS,
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None::<HMENU>,
            Some(hinstance),
            None,
        ) {
            SINGLETON_HWND.with(|c| *c.borrow_mut() = Some(hwnd));
        }
    }
}

unsafe extern "system" fn singleton_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
    if msg == WM_COPYDATA {
        let cds = lparam.0 as *const COPYDATASTRUCT;
        if !cds.is_null() {
            let cds = unsafe { &*cds };
            if cds.dwData == WM_COPYDATA_OPEN_URL && !cds.lpData.is_null() && cds.cbData > 0 {
                let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
                if let Ok(url) = std::str::from_utf8(bytes) {
                    let url = url.to_string();
                    ON_OPEN_URL.with(|c| {
                        if let Some(cb) = c.borrow().as_ref() {
                            cb(url);
                        }
                    });
                }
            }
            return LRESULT(1);
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Destroys the singleton window at shutdown.
pub fn destroy_singleton_window() {
    use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
    if let Some(hwnd) = SINGLETON_HWND.with(|c| c.borrow_mut().take()) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe fn append(menu: HMENU, item: &MenuItem, bitmaps: &mut Vec<HBITMAP>) {
    match item {
        MenuItem::Separator => {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        MenuItem::Entry { id, label, checked, enabled, icon } => {
            let mut flags = MF_STRING;
            if *checked {
                flags |= MF_CHECKED;
            }
            if !*enabled {
                flags |= MF_GRAYED | MF_DISABLED;
            }
            let text = HSTRING::from(label.as_str());
            let _ = AppendMenuW(menu, flags, *id as usize, &text);
            set_last_icon(menu, icon.as_ref(), bitmaps);
        }
        MenuItem::Submenu { label, items, icon } => {
            // Destroying the parent menu destroys attached submenus too.
            let Ok(sub) = CreatePopupMenu() else { return };
            for item in items {
                append(sub, item, bitmaps);
            }
            let text = HSTRING::from(label.as_str());
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &text);
            set_last_icon(menu, icon.as_ref(), bitmaps);
        }
    }
}

unsafe fn set_last_icon(menu: HMENU, icon: Option<&MenuIcon>, bitmaps: &mut Vec<HBITMAP>) {
    let Some(icon) = icon else { return };
    let Some(bitmap) = icon_bitmap(icon) else { return };
    let info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_BITMAP,
        hbmpItem: bitmap,
        ..Default::default()
    };
    let last = GetMenuItemCount(Some(menu)) - 1;
    let _ = SetMenuItemInfoW(menu, last as u32, true, &info);
    bitmaps.push(bitmap);
}

/// A 32-bit top-down DIB: menus draw such a bitmap with its alpha, which they take premultiplied.
unsafe fn icon_bitmap(icon: &MenuIcon) -> Option<HBITMAP> {
    if icon.size == 0 || icon.rgba.len() != (icon.size * icon.size * 4) as usize {
        return None;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: icon.size as i32,
            biHeight: -(icon.size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(bitmap.into());
        return None;
    }
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u8, icon.rgba.len());
    for (bgra, rgba) in pixels.chunks_exact_mut(4).zip(icon.rgba.chunks_exact(4)) {
        bgra.copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
    }
    Some(bitmap)
}
