//! Process- and OS-level Windows helpers: DPAPI, system memory, single instance, app identity,
//! native popup menus, the file-open dialog, and the clipboard. (Window/HWND glue lives in `win`.)

use std::cell::{Cell, RefCell};
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
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, DestroyMenu, FindWindowW, GetCursorPos, GetMenuItemCount,
    GetWindowThreadProcessId, SendMessageW, SetForegroundWindow, SetMenuItemInfoW, TrackPopupMenuEx, HMENU, MENUITEMINFOW, MF_CHECKED, MF_DISABLED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_BITMAP, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON,
    TPM_TOPALIGN, TRACK_POPUP_MENU_FLAGS, WM_COPYDATA,
};

/// A hidden window class/title used to find a running instance. Also the WM_COPYDATA tag.
const SINGLETON_CLASS: PCWSTR = windows::core::w!("LiteBrowser_Singleton");
pub const WM_COPYDATA_OPEN_URL: usize = 0x4C42; // 'LB'

/// Decrypts a DPAPI blob (CryptUnprotectData) for the current user. Used only for LiteBrowser's
/// own WebView2 key, to write imported passwords into our own store and show its saved ones.
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

// ---------------------------------------------------------------------------------------------
// Default browser

/// ProgID that http/https links point at once the user picks LiteBrowser.
const URL_PROGID: &str = "LiteBrowserURL";
const CLIENT_KEY: &str = r"Software\Clients\StartMenuInternet\LiteBrowser";

fn set_reg_string(subkey: &str, name: &str, value: &str) -> Result<(), String> {
    use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    let data: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let name = (!name.is_empty()).then(|| HSTRING::from(name));
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        )
    };
    err.ok().map_err(|e| format!(r"HKCU\{subkey}: {}", e.message()))
}

/// Lists LiteBrowser among the browsers in Windows' Default apps (per user, no admin rights) and
/// opens its page there. Since Windows 8 only the user may change the default browser, so this
/// is as far as an app can go. Rewritten on every call: a portable exe may have moved.
pub fn make_default_browser(exe: &std::path::Path) -> Result<(), String> {
    let exe = exe.display().to_string();
    let open = format!("\"{exe}\" \"%1\"");
    let icon = format!("\"{exe}\",0");
    let caps = format!(r"{CLIENT_KEY}\Capabilities");
    let progid = format!(r"Software\Classes\{URL_PROGID}");
    for (key, name, value) in [
        (progid.as_str(), "", "LiteBrowser URL"),
        (progid.as_str(), "URL Protocol", ""),
        (&format!(r"{progid}\DefaultIcon"), "", &icon),
        (&format!(r"{progid}\shell\open\command"), "", &open),
        (CLIENT_KEY, "", "LiteBrowser"),
        (&format!(r"{CLIENT_KEY}\DefaultIcon"), "", &icon),
        (&format!(r"{CLIENT_KEY}\shell\open\command"), "", &format!("\"{exe}\"")),
        (&caps, "ApplicationName", "LiteBrowser"),
        (&caps, "ApplicationDescription", "給記憶體很少的電腦用的輕量瀏覽器"),
        (&caps, "ApplicationIcon", &icon),
        (&format!(r"{caps}\URLAssociations"), "http", URL_PROGID),
        (&format!(r"{caps}\URLAssociations"), "https", URL_PROGID),
        (&format!(r"{caps}\StartMenu"), "StartMenuInternet", "LiteBrowser"),
        (r"Software\RegisteredApplications", "LiteBrowser", &caps),
    ] {
        set_reg_string(key, name, value)?;
    }
    unsafe {
        use windows::Win32::UI::Shell::{SHChangeNotify, ShellExecuteW, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
        // Windows 11 opens LiteBrowser's own page; Windows 10 ignores the argument and shows the
        // Default apps list, where the user picks the web browser.
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &HSTRING::from("ms-settings:defaultapps?registeredAppUser=LiteBrowser"),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
    Ok(())
}

/// A REG_SZ or REG_DWORD value as text; empty when it is missing.
fn reg_value(root: windows::Win32::System::Registry::HKEY, subkey: &str, name: &str) -> String {
    use windows::Win32::System::Registry::{RegGetValueW, REG_DWORD, REG_VALUE_TYPE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
    let mut buf = [0u16; 128];
    let mut len = std::mem::size_of_val(&buf) as u32;
    let mut kind = REG_VALUE_TYPE::default();
    let err = unsafe {
        RegGetValueW(
            root,
            &HSTRING::from(subkey),
            &HSTRING::from(name),
            RRF_RT_REG_SZ | RRF_RT_REG_DWORD,
            Some(&mut kind),
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if err.is_err() {
        String::new()
    } else if kind == REG_DWORD {
        (buf[0] as u32 | (buf[1] as u32) << 16).to_string()
    } else {
        String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1).min(buf.len())])
    }
}

/// Whether the user picked LiteBrowser for https links.
pub fn is_default_browser() -> bool {
    use windows::Win32::System::Registry::HKEY_CURRENT_USER;
    let key = r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice";
    reg_value(HKEY_CURRENT_USER, key, "ProgId") == URL_PROGID
}

/// E.g. "Windows 10 Enterprise 22H2, build 19045.5737".
pub fn os_version() -> String {
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let get = |name| reg_value(HKEY_LOCAL_MACHINE, key, name);
    let build = get("CurrentBuild");
    let mut product = get("ProductName");
    // ProductName still says Windows 10 on Windows 11, whose builds start at 22000.
    if build.parse::<u32>().is_ok_and(|b| b >= 22000) {
        product = product.replacen("Windows 10", "Windows 11", 1);
    }
    format!("{product} {}, build {build}.{}", get("DisplayVersion"), get("UBR"))
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
        // We were just launched by the user (a link clicked in another app), so we may hand the
        // foreground on; the running instance could not take it by itself.
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let _ = AllowSetForegroundWindow(pid);
        // Sent without a URL too: starting LiteBrowser again brings the running one up.
        let bytes = url.unwrap_or("").as_bytes();
        let cds = COPYDATASTRUCT {
            dwData: WM_COPYDATA_OPEN_URL,
            cbData: bytes.len() as u32,
            lpData: bytes.as_ptr() as *mut _,
        };
        SendMessageW(hwnd, WM_COPYDATA, Some(WPARAM(0)), Some(LPARAM(&cds as *const _ as isize)));
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
    static MENUS_DARK: Cell<bool> = const { Cell::new(false) };
}

/// Makes the native menus dark or light to match the browser. Windows has no public switch for
/// this; uxtheme's SetPreferredAppMode (ordinal 135, Windows 10 1903+) and FlushMenuThemes (136)
/// are what Explorer and Notepad use. Where they are missing the menus just stay light.
pub fn set_dark_menus(dark: bool) {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    if MENUS_DARK.replace(dark) == dark {
        return;
    }
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(windows::core::w!("uxtheme.dll")) else { return };
        // Ordinals travel in the name pointer's address, like MAKEINTRESOURCE.
        let ordinal = |n: usize| GetProcAddress(uxtheme, windows::core::PCSTR(std::ptr::without_provenance(n)));
        if let (Some(set_mode), Some(flush)) = (ordinal(135), ordinal(136)) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
            // 2 = ForceDark; 0 = Default, the light menus every app starts with.
            set_mode(if dark { 2 } else { 0 });
            flush();
        }
    }
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
// Clipboard (the passwords page's copy button)

/// Puts a password on the clipboard, kept out of Windows' clipboard history (Win+V) and cloud
/// clipboard sync, as password managers do: it would otherwise sit there in plain text for days.
pub fn copy_secret_to_clipboard(owner: HWND, text: &str) -> bool {
    use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW};
    const CF_UNICODETEXT: u32 = 13;
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            return false;
        }
        let exclude = RegisterClipboardFormatW(windows::core::w!("ExcludeClipboardContentFromMonitorProcessing"));
        let ok = EmptyClipboard().is_ok()
            && exclude != 0
            && set_clipboard_bytes(exclude, &[0u8; 4])
            && set_clipboard_bytes(
                CF_UNICODETEXT,
                std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2),
            );
        let _ = CloseClipboard();
        ok
    }
}

/// Copies `bytes` into a movable global block and hands it to the clipboard, which owns it from
/// then on; on failure the block is freed here.
unsafe fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> bool {
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::System::DataExchange::SetClipboardData;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    let Ok(block) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) else { return false };
    let dst = GlobalLock(block);
    if dst.is_null() {
        let _ = GlobalFree(Some(block));
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.cast::<u8>(), bytes.len());
    let _ = GlobalUnlock(block);
    if SetClipboardData(format, Some(HANDLE(block.0))).is_err() {
        let _ = GlobalFree(Some(block));
        return false;
    }
    true
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
            if cds.dwData == WM_COPYDATA_OPEN_URL {
                let bytes = if cds.lpData.is_null() || cds.cbData == 0 {
                    &[][..]
                } else {
                    unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) }
                };
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
    for (bgra, rgba) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(icon.rgba.as_chunks::<4>().0) {
        *bgra = [rgba[2], rgba[1], rgba[0], rgba[3]];
    }
    Some(bitmap)
}
