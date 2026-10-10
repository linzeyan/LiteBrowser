//! The browser: owns the tabs and WebViews, reacts to UI and WebView2 events.
//!
//! Everything runs on the UI thread. UI callbacks and WebView2 event handlers only `post()` an
//! [`Event`]; the queue is drained from a zero-delay Slint timer, so `App::handle` is never
//! re-entered while it is already running.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};
use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Controller, ICoreWebView2Environment};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use crate::adblock::Blocklist;
use crate::config::Config;
use crate::favicon::{self, FaviconCache};
use crate::import::{self, Profile};
use crate::logging::{self, log};
use crate::memory;
use crate::paths::Paths;
use crate::platform::{self, MenuItem};
use crate::shortcuts::{self, Shortcut};
use crate::storage::{
    self, Bookmarks, FolderEntry, History, Session, SessionTab, SuggestionKind, WindowState, ZoomLevels, OTHER_FOLDER,
};
use crate::tabs::{self, Action, MemoryState, Policy, Residency, TabId, TabSnapshot};
use crate::url_input;
use crate::webview::{self, EngineEvent, ExtensionChange, ExtensionInfo, NewWindowRequest, WebView};
use crate::{crx, devtools, mcp, update, win};

slint::include_modules!();

const WEBVIEW2_DOWNLOAD: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

// ---------------------------------------------------------------------------------------------
// Event queue

pub enum Event {
    Ui(UiEvent),
    Engine(TabId, EngineEvent),
    InitNative,
    EnvironmentReady(Result<ICoreWebView2Environment, String>),
    ViewCreated { tab: TabId, result: Result<ICoreWebView2Controller, String>, new_window: Option<NewWindowRequest> },
    ExtensionsListed(Result<Vec<ExtensionInfo>, String>),
    ExtensionInstalled { folder: String, result: Result<ExtensionInfo, String> },
    /// A Chrome Web Store download finished; the payload is the unpacked folder name.
    ExtensionDownloaded(Result<String, String>),
    ExtensionChanged(Result<(), String>),
    /// The docked DevTools controller finished being created.
    DevtoolsViewCreated(Result<ICoreWebView2Controller, String>),
    /// Each renderer PID with the ids of the frames it runs.
    RendererFrames(Vec<(u32, Vec<u32>)>),
    Tick,
}

pub enum UiEvent {
    Command(Shortcut),
    SelectTab(usize),
    CloseTab(usize),
    Navigate(String),
    OpenUrl(String),
    OpenUrlNewTab(String),
    /// An open tab was picked from the address bar's suggestions.
    SwitchToTab(TabId),
    Back,
    Forward,
    Reload,
    Stop,
    AddressEdited(String),
    AddressFocus(bool),
    CloseSuggestions,
    ShowPage(Page),
    RemoveBookmark(usize),
    SaveBookmark { index: usize, title: String, url: String, folder: String },
    BookmarkMenu(usize),
    /// A bookmarks-bar folder: its path and the chip's bottom-left corner, logical pixels.
    BookmarkFolderMenu(String, f32, f32),
    /// The bookmarks bar's » button: the first chip that did not fit, and the button's
    /// bottom-right corner, logical pixels.
    BookmarkOverflowMenu(usize, f32, f32),
    HistorySearch(String),
    RemoveHistory(String),
    ClearHistory,
    InstallExtension(String),
    ToggleExtension(String, bool),
    RemoveExtension(String),
    OpenExtensionsFolder,
    ReloadExtensions,
    OpenDataFolder,
    SaveSettings(SettingsData),
    DismissNotice,
    GeometryChanged,
    TabContextMenu(usize),
    /// The tab's speaker icon was clicked.
    ToggleMute(usize),
    /// The ⋮ button was clicked; its bottom-right corner in logical window coordinates.
    MainMenu(f32, f32),
    AddressMenu,
    ToggleDevtools,
    DevtoolsDockMenu,
    MinimizeWindow,
    ToggleMaximizeWindow,
    CloseWindow,
    RefreshImport,
    ImportRun { bookmarks: bool, history: bool },
    ImportPasswordsCsv,
    /// A second launch asked this instance to open a URL.
    OpenUrlFromOtherInstance(String),
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static QUEUE: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    static SCHEDULED: Cell<bool> = const { Cell::new(false) };
}

pub fn post(event: Event) {
    QUEUE.with(|q| q.borrow_mut().push_back(event));
    schedule(Duration::ZERO);
}

fn schedule(delay: Duration) {
    if !SCHEDULED.with(|s| s.replace(true)) {
        slint::Timer::single_shot(delay, process_queue);
    }
}

fn process_queue() {
    SCHEDULED.with(|s| s.set(false));
    while let Some(event) = QUEUE.with(|q| q.borrow_mut().pop_front()) {
        let leftover = APP.with(move |cell| match cell.try_borrow_mut() {
            Ok(mut app) => {
                if let Some(app) = app.as_mut() {
                    app.handle(event);
                }
                None
            }
            Err(_) => Some(event),
        });
        if let Some(event) = leftover {
            QUEUE.with(|q| q.borrow_mut().push_front(event));
            schedule(Duration::from_millis(1));
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Entry point

pub fn run() {
    let paths = Paths::detect();
    logging::init(&paths.log());
    log!("LiteBrowser {} starting; data dir {}", env!("CARGO_PKG_VERSION"), paths.root.display());

    // A URL passed on the command line (e.g. from "open with" or a second launch's forwarding).
    let launch_url = std::env::args().skip(1).find(|a| !a.starts_with('-'));

    // Single instance: a second launch forwards its URL to the first and exits.
    let instance = match platform::acquire_single_instance(launch_url.as_deref()) {
        Ok(guard) => Some(guard),
        Err(()) => {
            log!("another instance is running; forwarded URL and exiting");
            return;
        }
    };

    // Install a build downloaded last time, before any window or WebView2 exists.
    if let Ok(exe) = std::env::current_exe() {
        match update::apply_staged(&paths.root, &exe) {
            update::Applied::Replaced => {
                log!("installed a staged update; relaunching");
                // Let go of the single-instance mutex first: if the new build got there while we
                // still held it, it would take itself for a second launch and exit.
                drop(instance);
                let _ = std::process::Command::new(&exe).spawn();
                return;
            }
            update::Applied::Failed(e) => log!("update not installed: {e}"),
            update::Applied::Nothing => {}
        }
    }

    platform::set_app_id();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // Apply queued password imports before WebView2 locks its Login Data.
    apply_pending_password_imports(&paths);

    platform::create_singleton_window(|url| post(Event::Ui(UiEvent::OpenUrlFromOtherInstance(url))));

    let ui = match AppWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            log!("cannot create window: {e}");
            return;
        }
    };
    wire_callbacks(&ui);

    let app = App::new(ui.clone_strong(), paths, launch_url);
    APP.with(|cell| *cell.borrow_mut() = Some(app));

    start_mcp_server();
    start_update_check();

    let tick = slint::Timer::default();
    tick.start(slint::TimerMode::Repeated, Duration::from_secs(2), || post(Event::Tick));
    slint::Timer::single_shot(Duration::from_millis(20), || post(Event::InitNative));

    if let Err(e) = ui.run() {
        log!("event loop error: {e}");
    }

    tick.stop();
    if let Some(mut app) = APP.with(|cell| cell.borrow_mut().take()) {
        app.shutdown();
    }
    platform::destroy_singleton_window();
    log!("LiteBrowser exited");
}

/// Looks for a newer release in the background and stages it for the next start.
fn start_update_check() {
    let (enabled, root) = APP.with(|cell| {
        let app = cell.borrow();
        let app = app.as_ref().expect("app exists before the event loop runs");
        (app.cfg.auto_update, app.paths.root.clone())
    });
    if !enabled {
        return;
    }
    std::thread::spawn(move || {
        let current = env!("CARGO_PKG_VERSION");
        match update::check(current) {
            Ok(Some(release)) => {
                log!("update available: {}", release.version);
                match update::download(&release, &root) {
                    Ok(()) => {
                        log!("update {} downloaded", release.version);
                        let version = release.version.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            APP.with(|cell| {
                                if let Some(app) = cell.borrow_mut().as_mut() {
                                    app.ui.set_notice(
                                        format!("已下載新版 {version}，重新啟動 LiteBrowser 即可更新。").into(),
                                    );
                                }
                            });
                        });
                    }
                    Err(e) => log!("update download failed: {e}"),
                }
            }
            Ok(None) => log!("no update available"),
            Err(e) => log!("update check failed: {e}"),
        }
    });
}

/// Starts the MCP endpoint when it is enabled, and records the URL an LLM client should use.
fn start_mcp_server() {
    let (enabled, host, port, paths) = APP.with(|cell| {
        let app = cell.borrow();
        let app = app.as_ref().expect("app exists before the event loop runs");
        (app.cfg.mcp_enabled, app.cfg.mcp_host.clone(), app.cfg.mcp_port, app.paths.clone())
    });
    if !enabled {
        return;
    }
    // Config::sanitized only lets IP addresses through.
    let host: std::net::IpAddr = host.parse().unwrap_or(std::net::Ipv4Addr::LOCALHOST.into());
    let token = mcp::generate_token();
    match mcp::serve((host, port).into(), token.clone(), std::sync::Arc::new(mcp_dispatch)) {
        Ok(mut bound) => {
            // 0.0.0.0 cannot be dialed; a client on this machine reaches it through loopback.
            if bound.ip().is_unspecified() {
                bound.set_ip(match bound.ip() {
                    std::net::IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    std::net::IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                });
            }
            let url = format!("http://{bound}/mcp/{token}");
            log!("MCP endpoint listening on {url}");
            // The token is new on every run, so write the current URL out for client config.
            let _ = crate::paths::write_atomic(&paths.mcp_url(), url.as_bytes());
            APP.with(|cell| {
                if let Some(app) = cell.borrow_mut().as_mut() {
                    app.mcp_url = url.clone();
                    app.ui.set_mcp_url(url.into());
                }
            });
        }
        Err(e) => log!("MCP endpoint failed to start: {e}"),
    }
}

/// Writes any CSV-exported passwords queued earlier into our own WebView2 store, before the
/// environment is created (WebView2 locks `Login Data` once it runs).
fn apply_pending_password_imports(paths: &Paths) {
    let pending_path = paths.root.join("pending-import.json");
    let pending = import::PendingImport::load(&pending_path);
    if pending.password_csvs.is_empty() {
        return;
    }
    let login_data = paths.webview_data().join("EBWebView/Default/Login Data");
    let local_state = paths.webview_data().join("EBWebView/Local State");
    let Some(key) = import::crypto::key_from_local_state(&local_state, &platform::dpapi_unprotect) else {
        log!("pending password import deferred: WebView2 key not available yet");
        return;
    };
    let mut logins = Vec::new();
    for csv in &pending.password_csvs {
        match import::csv::read_logins_file(csv) {
            Ok(mut found) => logins.append(&mut found),
            Err(e) => log!("password CSV {}: {e}", csv.display()),
        }
    }
    match import::login_db::apply(&login_data, &key, &logins) {
        Ok(applied) => {
            log!("imported {} passwords ({} already present)", applied.added, applied.skipped_existing);
            let _ = import::PendingImport::default().save(&pending_path);
        }
        Err(e) => log!("password import deferred: {e}"),
    }
}

// Icons.folder and Icons.globe in app.slint, for native menus.
const FOLDER_GLYPH: &str = "M 3.5 6.5 A 1.5 1.5 0 0 1 5 5 L 9.5 5 L 11.5 7 L 19 7 A 1.5 1.5 0 0 1 20.5 8.5 L 20.5 17.5 A 1.5 1.5 0 0 1 19 19 L 5 19 A 1.5 1.5 0 0 1 3.5 17.5 Z";
const GLOBE_GLYPH: &str = "M 3.5 12 A 8.5 8.5 0 1 0 20.5 12 A 8.5 8.5 0 1 0 3.5 12 M 3.5 12 L 20.5 12 M 12 3.5 A 5 8.5 0 0 0 12 20.5 A 5 8.5 0 0 0 12 3.5";

/// What a bookmark menu's icons are drawn with, at the menu's pixel size.
struct MenuIcons<'a> {
    favicons: &'a mut FaviconCache,
    px: u32,
    folder: Option<platform::MenuIcon>,
    globe: Option<platform::MenuIcon>,
}

/// Menu items for bookmark-folder entries, subfolders as submenus, each with its icon like on the
/// bookmarks bar. Item ids are bookmark index + 1.
fn bookmark_menu_items(bookmarks: &Bookmarks, icons: &mut MenuIcons, entries: Vec<FolderEntry>) -> Vec<MenuItem> {
    entries
        .into_iter()
        .map(|entry| match entry {
            FolderEntry::Bookmark(i) => {
                let b = &bookmarks.items()[i];
                let icon = icons.favicons.get(&b.url).and_then(|img| favicon::menu_icon(&img, icons.px));
                MenuItem::entry(i as u32 + 1, b.title.replace('&', "&&")).with_icon(icon.or_else(|| icons.globe.clone()))
            }
            FolderEntry::Folder(sub, name) => MenuItem::Submenu {
                label: name.replace('&', "&&"),
                items: bookmark_menu_items(bookmarks, icons, bookmarks.folder_entries(&sub)),
                icon: icons.folder.clone(),
            },
        })
        .collect()
}

fn wire_callbacks(ui: &AppWindow) {
    fn ui_post(event: UiEvent) {
        post(Event::Ui(event));
    }
    ui.on_command(|name| {
        if let Some(s) = shortcuts::from_name(&name) {
            ui_post(UiEvent::Command(s));
        }
    });
    ui.on_select_tab(|i| ui_post(UiEvent::SelectTab(i.max(0) as usize)));
    ui.on_close_tab(|i| ui_post(UiEvent::CloseTab(i.max(0) as usize)));
    ui.on_tab_context_menu(|i| ui_post(UiEvent::TabContextMenu(i.max(0) as usize)));
    ui.on_toggle_tab_mute(|i| ui_post(UiEvent::ToggleMute(i.max(0) as usize)));
    ui.on_main_menu(|x, y| ui_post(UiEvent::MainMenu(x, y)));
    ui.on_address_menu(|| ui_post(UiEvent::AddressMenu));
    ui.on_navigate(|text| ui_post(UiEvent::Navigate(text.into())));
    ui.on_open_url(|url| ui_post(UiEvent::OpenUrl(url.into())));
    ui.on_open_url_new_tab(|url| ui_post(UiEvent::OpenUrlNewTab(url.into())));
    ui.on_switch_to_tab(|id| ui_post(UiEvent::SwitchToTab(id.max(0) as TabId)));
    ui.on_go_back(|| ui_post(UiEvent::Back));
    ui.on_go_forward(|| ui_post(UiEvent::Forward));
    ui.on_reload(|| ui_post(UiEvent::Reload));
    ui.on_stop(|| ui_post(UiEvent::Stop));
    ui.on_address_edited(|text| ui_post(UiEvent::AddressEdited(text.into())));
    ui.on_address_focus_changed(|focused| ui_post(UiEvent::AddressFocus(focused)));
    ui.on_close_suggestions(|| ui_post(UiEvent::CloseSuggestions));
    ui.on_show_page(|page| ui_post(UiEvent::ShowPage(page)));
    ui.on_remove_bookmark(|i| ui_post(UiEvent::RemoveBookmark(i.max(0) as usize)));
    ui.on_save_bookmark(|i, title, url, folder| {
        ui_post(UiEvent::SaveBookmark { index: i.max(0) as usize, title: title.into(), url: url.into(), folder: folder.into() })
    });
    ui.on_bookmark_menu(|i| ui_post(UiEvent::BookmarkMenu(i.max(0) as usize)));
    ui.on_bookmark_folder_menu(|path, x, y| ui_post(UiEvent::BookmarkFolderMenu(path.into(), x, y)));
    ui.on_bookmark_overflow_menu(|first, x, y| ui_post(UiEvent::BookmarkOverflowMenu(first.max(0) as usize, x, y)));
    ui.on_history_search(|q| ui_post(UiEvent::HistorySearch(q.into())));
    ui.on_remove_history(|url| ui_post(UiEvent::RemoveHistory(url.into())));
    ui.on_clear_history(|| ui_post(UiEvent::ClearHistory));
    ui.on_install_extension(|url| ui_post(UiEvent::InstallExtension(url.into())));
    ui.on_toggle_extension(|id, enabled| ui_post(UiEvent::ToggleExtension(id.into(), enabled)));
    ui.on_remove_extension(|id| ui_post(UiEvent::RemoveExtension(id.into())));
    ui.on_open_extensions_folder(|| ui_post(UiEvent::OpenExtensionsFolder));
    ui.on_reload_extensions(|| ui_post(UiEvent::ReloadExtensions));
    ui.on_open_data_folder(|| ui_post(UiEvent::OpenDataFolder));
    ui.on_save_settings(|s| ui_post(UiEvent::SaveSettings(s)));
    ui.on_dismiss_notice(|| ui_post(UiEvent::DismissNotice));
    ui.on_content_geometry_changed(|| ui_post(UiEvent::GeometryChanged));
    ui.on_toggle_devtools(|| ui_post(UiEvent::ToggleDevtools));
    ui.on_devtools_dock_menu(|| ui_post(UiEvent::DevtoolsDockMenu));
    ui.on_minimize_window(|| ui_post(UiEvent::MinimizeWindow));
    ui.on_toggle_maximize_window(|| ui_post(UiEvent::ToggleMaximizeWindow));
    ui.on_close_window(|| ui_post(UiEvent::CloseWindow));
    ui.on_refresh_import(|| ui_post(UiEvent::RefreshImport));
    ui.on_import_run(|bookmarks, history| ui_post(UiEvent::ImportRun { bookmarks, history }));
    ui.on_import_passwords_csv(|| ui_post(UiEvent::ImportPasswordsCsv));
}

// ---------------------------------------------------------------------------------------------
// MCP bridge
//
// The MCP server runs on its own thread, but WebView2 may only be touched on the UI thread, so
// every call hops across with `invoke_from_event_loop` and waits for the answer on a channel.

type McpAnswer = std::sync::mpsc::Sender<Result<serde_json::Value, String>>;

fn mcp_dispatch(call: mcp::Call) -> Result<serde_json::Value, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    slint::invoke_from_event_loop(move || {
        let answered = APP.with(|cell| match cell.try_borrow_mut() {
            Ok(mut app) => match app.as_mut() {
                Some(app) => {
                    app.run_mcp_call(call, tx.clone());
                    true
                }
                None => false,
            },
            Err(_) => false,
        });
        if !answered {
            let _ = tx.send(Err("瀏覽器尚未就緒".into()));
        }
    })
    .map_err(|e| format!("無法送到瀏覽器：{e}"))?;
    // Long enough for a page to load and a script to run, short enough not to hang the model.
    rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "瀏覽器沒有在時間內回應".to_string())?
}

// ---------------------------------------------------------------------------------------------
// App state

struct Tab {
    id: TabId,
    url: String,
    title: String,
    pinned: bool,
    last_active_ms: u64,
    loading: bool,
    can_back: bool,
    can_forward: bool,
    view: Option<WebView>,
    /// A controller is being created for this tab.
    creating: bool,
    /// Frozen with TrySuspend (or asked to be).
    suspended: bool,
    /// The tab whose page opened this one (window.open / target=_blank).
    opener: Option<TabId>,
    /// Last URL written to history, so a reload or repeated event is not counted twice.
    recorded_url: String,
    /// The site icon, if known (empty image otherwise).
    favicon: Image,
    /// A private tab: in-memory profile, and nothing is written to history or the session.
    private: bool,
    /// Memory of the renderers running this tab, from the last measurement; 0 when unknown.
    bytes: u64,
    /// The page last reported, when it went to the background, that it holds text the user
    /// has not sent.
    unsaved: bool,
    /// The page plays sound (even while muted).
    audible: bool,
    /// The user muted the tab; outlives its WebView, like its URL.
    muted: bool,
}

impl Tab {
    fn new(id: TabId, url: String, title: String) -> Self {
        Self {
            id,
            url,
            title,
            pinned: false,
            last_active_ms: 0,
            loading: false,
            can_back: false,
            can_forward: false,
            view: None,
            creating: false,
            suspended: false,
            opener: None,
            recorded_url: String::new(),
            favicon: Image::default(),
            private: false,
            bytes: 0,
            unsaved: false,
            audible: false,
            muted: false,
        }
    }

    /// Lets go of the WebView and of everything that only lived in its page.
    fn drop_view(&mut self) {
        self.view = None;
        self.suspended = false;
        self.loading = false;
        self.bytes = 0;
        self.unsaved = false;
        self.audible = false;
    }

    fn residency(&self) -> Residency {
        if self.view.is_none() && !self.creating {
            Residency::Discarded
        } else if self.suspended {
            Residency::Suspended
        } else {
            Residency::Live
        }
    }
}

struct Models {
    tabs: Rc<VecModel<TabData>>,
    suggestions: Rc<VecModel<SuggestionData>>,
    bookmarks: Rc<VecModel<LinkData>>,
    bar: Rc<VecModel<BarItemData>>,
    history: Rc<VecModel<LinkData>>,
    top_sites: Rc<VecModel<LinkData>>,
    extensions: Rc<VecModel<ExtensionData>>,
    import_profiles: Rc<VecModel<ImportProfileData>>,
}

struct App {
    ui: AppWindow,
    paths: Paths,
    cfg: Config,
    models: Models,
    hwnd: Option<HWND>,
    init_attempts: u32,
    env: Option<ICoreWebView2Environment>,
    blocklist: Rc<Blocklist>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: TabId,
    closed: Vec<SessionTab>,
    bookmarks: Bookmarks,
    history: History,
    zoom: ZoomLevels,
    page: Page,
    suggestions_open: bool,
    address_focused: bool,
    /// The user typed into the address bar since it last showed the tab's URL.
    address_edited: bool,
    history_query: String,
    usage: memory::Usage,
    started: Instant,
    last_save_ms: u64,
    session_dirty: bool,
    minimized: bool,
    /// The active page asked for full screen (a video player).
    page_fullscreen: bool,
    /// The user pressed F11.
    user_fullscreen: bool,
    /// Set when the browser is quitting because its last tab was closed: the next start is blank.
    forget_session: bool,
    /// Extension folder name → installed extension id.
    installed_extensions: BTreeMap<String, String>,
    extensions_synced: bool,
    /// A store download is in flight; the install button stays disabled until it lands.
    extension_installing: bool,
    status_message: String,
    status_message_ms: u64,
    window_state: Option<WindowState>,
    favicons: FaviconCache,
    system_memory_free: u64,
    /// Detected browser profiles for the import page, in UI order.
    import_profiles: Vec<Profile>,
    /// The docked DevTools WebView, once attached.
    devtools_view: Option<WebView>,
    devtools_creating: bool,
    pending_devtools_url: Option<String>,
    launch_url: Option<String>,
    /// The MCP endpoint URL, when the server is running.
    mcp_url: String,
}

impl App {
    fn new(ui: AppWindow, paths: Paths, launch_url: Option<String>) -> Self {
        let (cfg, cfg_error) = match Config::load(&paths.config()) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(e)),
        };
        let models = Models {
            tabs: Rc::new(VecModel::default()),
            suggestions: Rc::new(VecModel::default()),
            bookmarks: Rc::new(VecModel::default()),
            bar: Rc::new(VecModel::default()),
            history: Rc::new(VecModel::default()),
            top_sites: Rc::new(VecModel::default()),
            extensions: Rc::new(VecModel::default()),
            import_profiles: Rc::new(VecModel::default()),
        };
        ui.set_tabs(ModelRc::from(models.tabs.clone()));
        ui.set_suggestions(ModelRc::from(models.suggestions.clone()));
        ui.set_bookmark_items(ModelRc::from(models.bookmarks.clone()));
        ui.set_bar_items(ModelRc::from(models.bar.clone()));
        ui.set_history_items(ModelRc::from(models.history.clone()));
        ui.set_top_sites(ModelRc::from(models.top_sites.clone()));
        ui.set_extensions(ModelRc::from(models.extensions.clone()));
        ui.set_import_profiles(ModelRc::from(models.import_profiles.clone()));
        ui.set_data_dir(paths.root.display().to_string().into());
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());

        let installed_extensions = std::fs::read(paths.root.join("extensions.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();

        let favicons = FaviconCache::new(paths.root.join("favicons"));
        let mut app = Self {
            blocklist: Rc::new(Blocklist::load(&paths.blocklist())),
            bookmarks: Bookmarks::load(&paths.bookmarks()),
            history: History::load(&paths.history()),
            zoom: ZoomLevels::load(&paths.zoom()),
            favicons,
            ui,
            paths,
            cfg,
            models,
            hwnd: None,
            init_attempts: 0,
            env: None,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            closed: Vec::new(),
            page: Page::NewTab,
            suggestions_open: false,
            address_focused: false,
            address_edited: false,
            history_query: String::new(),
            usage: memory::Usage::default(),
            started: Instant::now(),
            last_save_ms: 0,
            session_dirty: false,
            minimized: false,
            page_fullscreen: false,
            user_fullscreen: false,
            forget_session: false,
            installed_extensions,
            extensions_synced: false,
            extension_installing: false,
            status_message: String::new(),
            status_message_ms: 0,
            window_state: None,
            system_memory_free: 0,
            import_profiles: Vec::new(),
            devtools_view: None,
            devtools_creating: false,
            pending_devtools_url: None,
            launch_url,
            mcp_url: String::new(),
        };
        if let Some(e) = cfg_error {
            app.ui.set_notice(e.into());
        }

        let mut session = Session::load(&app.paths.session());
        app.window_state = session.window.take();
        app.restore_window();
        if !app.cfg.restore_session {
            session.tabs.clear();
        }
        for t in session.tabs.iter().filter(|t| !t.url.is_empty()) {
            let idx = app.push_tab(t.url.clone(), t.title.clone());
            app.tabs[idx].pinned = t.pinned;
        }
        // A URL from the command line opens in a fresh foreground tab.
        if let Some(url) = app.launch_url.take().and_then(|u| url_input::to_url(&u, &app.cfg.search_url)) {
            let at = app.tabs.len();
            app.insert_tab(at, url, String::new(), None);
            session.active = at;
        }
        if app.tabs.is_empty() {
            app.push_tab(String::new(), String::new());
        }
        app.reorder_pinned_first();
        app.active = session.active.min(app.tabs.len() - 1);
        app.page = if app.tabs[app.active].url.is_empty() { Page::NewTab } else { Page::Web };
        app.ui.set_settings(app.settings_data());
        app.ui.set_devtools_right(app.cfg.devtools_dock_right);
        app.ui.set_window_maximized(app.ui.window().is_maximized());
        app.refresh_bookmarks();
        app.refresh_top_sites();
        app.refresh_all();
        app
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Ui(e) => self.handle_ui(e),
            Event::Engine(tab, e) => self.handle_engine(tab, e),
            Event::InitNative => self.init_native(),
            Event::EnvironmentReady(result) => self.on_environment(result),
            Event::ViewCreated { tab, result, new_window } => self.on_view_created(tab, result, new_window),
            Event::ExtensionsListed(result) => self.on_extensions_listed(result),
            Event::ExtensionInstalled { folder, result } => self.on_extension_installed(folder, result),
            Event::ExtensionDownloaded(result) => self.on_extension_downloaded(result),
            Event::ExtensionChanged(result) => {
                if let Err(e) = result {
                    self.ui.set_extensions_message(format!("操作失敗：{e}").into());
                }
                self.request_extension_list();
            }
            Event::DevtoolsViewCreated(result) => self.on_devtools_view_created(result),
            Event::RendererFrames(renderers) => self.on_renderer_frames(renderers),
            Event::Tick => self.tick(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Startup / shutdown

    fn init_native(&mut self) {
        let Some(hwnd) = win::hwnd_of(self.ui.window()) else {
            // The native window appears after the first event loop iterations.
            self.init_attempts += 1;
            if self.init_attempts < 200 {
                slint::Timer::single_shot(Duration::from_millis(50), || post(Event::InitNative));
            } else {
                log!("no native window handle");
            }
            return;
        };
        self.hwnd = Some(hwnd);
        win::prepare_main_window(hwnd);
        platform::set_menu_owner(hwnd);
        self.create_environment();
    }

    fn create_environment(&mut self) {
        // Always enable the remote-debugging server so DevTools can attach on demand (localhost only).
        let args = format!("{} {}", self.cfg.browser_args(), devtools::browser_switches());
        log!("creating WebView2 environment with args: {args}");
        let result = webview::create_environment(&self.paths.webview_data(), args, |result| {
            post(Event::EnvironmentReady(result));
        });
        if let Err(e) = result {
            self.on_environment(Err(e));
        }
    }

    fn on_environment(&mut self, result: Result<ICoreWebView2Environment, String>) {
        match result {
            Ok(env) => {
                log!("WebView2 environment ready");
                self.env = Some(env);
                let active = self.active;
                self.activate(active);
            }
            Err(e) => {
                log!("WebView2 environment failed: {e}");
                self.ui.set_notice(
                    format!(
                        "無法啟動 Microsoft Edge WebView2：{e}\n請確認已安裝 WebView2 Runtime（{WEBVIEW2_DOWNLOAD}），然後重新開啟 LiteBrowser。"
                    )
                    .into(),
                );
            }
        }
    }

    fn shutdown(&mut self) {
        self.save_all();
        // Dropping the WebViews closes their controllers.
        self.devtools_view = None;
        self.tabs.clear();
        win::set_active_controller(None);
    }

    fn save_all(&mut self) {
        self.bookmarks.save_if_dirty();
        self.history.save_if_dirty();
        self.zoom.save_if_dirty();
        self.save_session();
        self.last_save_ms = self.now_ms();
    }

    /// First start: maximized (VDI screens are small). Later: as the user left it.
    fn restore_window(&self) {
        let window = self.ui.window();
        let (size, maximized) = match &self.window_state {
            Some(s) if s.width >= 400.0 && s.height >= 300.0 => ((s.width, s.height), s.maximized),
            Some(s) => ((1280.0, 820.0), s.maximized),
            None => ((1280.0, 820.0), true),
        };
        // Always give an explicit size: without one Slint resizes the window to its preferred
        // size when it is first shown, and that resize takes it out of the maximized state.
        window.set_size(slint::LogicalSize::new(size.0, size.1));
        window.set_maximized(maximized);
    }

    fn capture_window_state(&mut self) {
        let window = self.ui.window();
        if window.is_minimized() {
            return;
        }
        let maximized = window.is_maximized();
        let (width, height) = match (&self.window_state, maximized) {
            (Some(old), true) => (old.width, old.height),
            (None, true) => (1280.0, 820.0),
            (_, false) => {
                let size = window.size().to_logical(window.scale_factor());
                (size.width, size.height)
            }
        };
        self.window_state = Some(WindowState { width, height, maximized });
    }

    fn save_session(&mut self) {
        self.capture_window_state();
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .filter(|t| !t.url.is_empty() && !t.private)
            .map(|t| SessionTab { url: t.url.clone(), title: t.title.clone(), pinned: t.pinned })
            .collect();
        let active = self.tabs.iter().take(self.active).filter(|t| !t.url.is_empty()).count();
        let (tabs, active) = if self.forget_session { (Vec::new(), 0) } else { (tabs, active) };
        Session { tabs, active, window: self.window_state.clone() }.save(&self.paths.session());
        self.session_dirty = false;
    }

    // -----------------------------------------------------------------------------------------
    // UI events

    fn handle_ui(&mut self, event: UiEvent) {
        match event {
            UiEvent::Command(s) => self.run_shortcut(s),
            UiEvent::SelectTab(i) => self.activate(i),
            UiEvent::CloseTab(i) => self.close_tab(i),
            UiEvent::TabContextMenu(i) => self.tab_context_menu(i),
            UiEvent::ToggleMute(i) => self.toggle_mute(i),
            UiEvent::MainMenu(x, y) => self.main_menu(x, y),
            UiEvent::AddressMenu => self.address_menu(),
            UiEvent::Navigate(text) => {
                if let Some(url) = url_input::to_url(&text, &self.cfg.search_url) {
                    self.navigate_active(url);
                }
            }
            UiEvent::OpenUrl(url) => self.open_url(url),
            UiEvent::OpenUrlNewTab(url) => {
                let at = self.active + 1;
                self.insert_tab(at, url, String::new(), None);
                self.refresh_tabs();
                self.refresh_status();
            }
            UiEvent::SwitchToTab(id) => {
                if let Some(idx) = self.index_of(id) {
                    self.activate(idx);
                }
            }
            UiEvent::Back => self.with_active_view(WebView::go_back),
            UiEvent::Forward => self.with_active_view(WebView::go_forward),
            UiEvent::Reload => self.reload_active(),
            UiEvent::Stop => self.with_active_view(WebView::stop),
            UiEvent::AddressEdited(text) => self.on_address_edited(text),
            UiEvent::AddressFocus(focused) => {
                self.address_focused = focused;
                // No SetFocus here: clicks on the bar already take focus (win.rs), and when the
                // window is re-activated Slint re-focuses the bar on its own, so taking focus
                // here stole a click that had just landed in the page.
                if !focused && self.suggestions_open {
                    self.set_suggestions_open(false);
                }
            }
            UiEvent::CloseSuggestions => {
                self.address_edited = false;
                self.set_suggestions_open(false);
                self.sync_address();
                if self.page == Page::Web {
                    self.with_active_view(WebView::focus);
                }
            }
            UiEvent::ShowPage(page) => self.show_page(page),
            UiEvent::RemoveBookmark(i) => {
                self.bookmarks.remove(i);
                self.bookmarks.save_if_dirty();
                self.refresh_bookmarks();
                self.refresh_toolbar();
            }
            UiEvent::SaveBookmark { index, title, url, folder } => {
                self.bookmarks.update(index, &title, &url, &folder);
                self.bookmarks.save_if_dirty();
                self.refresh_bookmarks();
                self.refresh_toolbar();
            }
            UiEvent::BookmarkMenu(i) => self.bookmark_menu(i),
            UiEvent::BookmarkFolderMenu(path, x, y) => self.bookmark_folder_menu(&path, x, y),
            UiEvent::BookmarkOverflowMenu(first, x, y) => self.bookmark_overflow_menu(first, x, y),
            UiEvent::HistorySearch(q) => {
                self.history_query = q;
                self.refresh_history();
            }
            UiEvent::RemoveHistory(url) => {
                self.history.remove_url(&url);
                self.refresh_history();
                self.refresh_top_sites();
            }
            UiEvent::ClearHistory => {
                self.history.clear();
                self.history.save_if_dirty();
                self.refresh_history();
                self.refresh_top_sites();
            }
            UiEvent::InstallExtension(url) => self.install_from_store(&url),
            UiEvent::ToggleExtension(id, enabled) => self.change_extension(id, ExtensionChange::Enable(enabled)),
            UiEvent::RemoveExtension(id) => self.change_extension(id, ExtensionChange::Remove),
            UiEvent::OpenExtensionsFolder => win::shell_open(&self.paths.extensions_dir()),
            UiEvent::ReloadExtensions => self.sync_extensions(true),
            UiEvent::OpenDataFolder => win::shell_open(&self.paths.root),
            UiEvent::SaveSettings(s) => self.save_settings(s),
            UiEvent::DismissNotice => self.ui.set_notice(SharedString::new()),
            UiEvent::GeometryChanged => self.layout_views(),
            UiEvent::ToggleDevtools => self.toggle_devtools(),
            UiEvent::DevtoolsDockMenu => self.devtools_dock_menu(),
            UiEvent::MinimizeWindow => self.ui.window().set_minimized(true),
            UiEvent::ToggleMaximizeWindow => {
                let maximized = !self.ui.window().is_maximized();
                self.ui.window().set_maximized(maximized);
                self.ui.set_window_maximized(maximized);
            }
            UiEvent::CloseWindow => self.quit(),
            UiEvent::RefreshImport => self.refresh_import(),
            UiEvent::ImportRun { bookmarks, history } => self.run_import(bookmarks, history),
            UiEvent::ImportPasswordsCsv => self.import_passwords_csv(),
            UiEvent::OpenUrlFromOtherInstance(url) => {
                if let Some(hwnd) = self.hwnd {
                    win::focus_main_window(hwnd);
                }
                if let Some(url) = url_input::to_url(&url, &self.cfg.search_url) {
                    let at = self.active + 1;
                    let idx = self.insert_tab(at, url, String::new(), None);
                    self.activate(idx);
                }
            }
        }
    }

    fn run_shortcut(&mut self, shortcut: Shortcut) {
        let count = self.tabs.len();
        match shortcut {
            Shortcut::NewTab => {
                let idx = self.insert_tab(count, String::new(), String::new(), None);
                self.activate(idx);
            }
            Shortcut::NewPrivateTab => {
                let idx = self.insert_tab(count, String::new(), String::new(), None);
                self.tabs[idx].private = true;
                self.activate(idx);
                self.set_status("無痕分頁：不會留下 Cookie、歷史紀錄或快取");
            }
            Shortcut::CloseTab => self.close_tab(self.active),
            Shortcut::ReopenClosedTab => {
                if let Some(t) = self.closed.pop() {
                    let idx = self.insert_tab(self.active + 1, t.url, t.title, None);
                    self.tabs[idx].pinned = t.pinned;
                    self.activate(idx);
                }
            }
            Shortcut::NextTab => self.activate((self.active + 1) % count),
            Shortcut::PrevTab => self.activate((self.active + count - 1) % count),
            Shortcut::SelectTab(i) => {
                if i < count {
                    self.activate(i);
                }
            }
            Shortcut::LastTab => self.activate(count - 1),
            Shortcut::FocusAddress => self.focus_address(),
            Shortcut::ToggleBookmark => {
                let tab = &self.tabs[self.active];
                if storage::is_recordable(&tab.url) {
                    let added = self.bookmarks.toggle(&tab.url.clone(), &tab.title.clone());
                    self.bookmarks.save_if_dirty();
                    self.set_status(if added { "已加入書籤" } else { "已移除書籤" });
                    self.refresh_bookmarks();
                    self.refresh_toolbar();
                }
            }
            Shortcut::ShowBookmarks => self.show_page(Page::Bookmarks),
            Shortcut::ShowHistory => self.show_page(Page::History),
            Shortcut::ShowDownloads => {
                if self.page != Page::Web {
                    let active = self.active;
                    self.activate(active);
                }
                let opened = self.tabs[self.active].view.as_ref().is_some_and(WebView::open_downloads);
                if !opened {
                    self.set_status("下載清單要在網頁分頁中開啟");
                }
            }
            Shortcut::Reload => self.reload_active(),
            Shortcut::ToggleFullScreen => {
                self.user_fullscreen = !self.user_fullscreen;
                self.apply_fullscreen();
            }
            Shortcut::ExitFullScreen => {
                if self.user_fullscreen {
                    self.user_fullscreen = false;
                    self.apply_fullscreen();
                }
            }
            Shortcut::ToggleDevtools => self.toggle_devtools(),
            Shortcut::ResetZoom => self.set_site_zoom(self.active, 1.0),
        }
    }

    fn reload_active(&mut self) {
        match self.page {
            Page::Web => {
                let tab = &self.tabs[self.active];
                if tab.view.is_some() {
                    self.with_active_view(WebView::reload);
                } else if !tab.url.is_empty() {
                    self.ensure_view(self.active);
                }
            }
            Page::History => self.refresh_history(),
            Page::Extensions => self.request_extension_list(),
            _ => {}
        }
    }

    fn with_active_view(&self, f: impl FnOnce(&WebView)) {
        if let Some(view) = self.tabs.get(self.active).and_then(|t| t.view.as_ref()) {
            f(view);
        }
    }

    fn focus_address(&mut self) {
        if let Some(hwnd) = self.hwnd {
            win::focus_main_window(hwnd);
        }
        self.ui.invoke_focus_address();
    }

    fn set_status(&mut self, message: &str) {
        self.status_message = message.to_string();
        self.status_message_ms = self.now_ms();
        self.refresh_status();
    }

    // -----------------------------------------------------------------------------------------
    // Tabs

    fn push_tab(&mut self, url: String, title: String) -> usize {
        let at = self.tabs.len();
        self.insert_tab(at, url, title, None)
    }

    fn insert_tab(&mut self, at: usize, url: String, title: String, opener: Option<TabId>) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let mut tab = Tab::new(id, url, title);
        tab.opener = opener;
        tab.last_active_ms = self.now_ms();
        tab.favicon = self.favicons.get(&tab.url).unwrap_or_default();
        let at = at.min(self.tabs.len());
        self.tabs.insert(at, tab);
        if at <= self.active && self.tabs.len() > 1 {
            self.active += 1;
        }
        self.session_dirty = true;
        at
    }

    fn index_of(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn activate(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        let now = self.now_ms();
        if idx != self.active {
            if let Some(old) = self.tabs.get_mut(self.active) {
                old.last_active_ms = now;
            }
            self.active = idx;
            // A video that was playing full screen is no longer the tab on screen.
            if self.page_fullscreen {
                self.page_fullscreen = false;
                self.apply_fullscreen();
            }
        }
        self.suggestions_open = false;
        self.address_edited = false;
        let tab = &mut self.tabs[idx];
        tab.last_active_ms = now;
        self.page = if tab.url.is_empty() { Page::NewTab } else { Page::Web };
        if let Some(view) = &tab.view {
            if tab.suspended {
                view.resume();
                tab.suspended = false;
            }
            view.set_low_memory(false);
        }
        self.ensure_view(idx);
        self.layout_views();
        if self.page == Page::NewTab {
            self.refresh_top_sites();
            self.focus_address();
        } else {
            self.with_active_view(WebView::focus);
        }
        self.session_dirty = true;
        self.refresh_all();
        self.apply_policy(None);
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        // Closing the last tab closes the browser (like Chrome). Quitting this way means the user
        // finished with those tabs, so the next start is blank; closing the window with X restores.
        if self.tabs.len() == 1 {
            self.forget_session = true;
            self.save_all();
            slint::quit_event_loop().ok();
            return;
        }
        let tab = self.tabs.remove(idx);
        if !tab.url.is_empty() && !tab.private {
            self.closed.push(SessionTab { url: tab.url.clone(), title: tab.title.clone(), pinned: tab.pinned });
            if self.closed.len() > 25 {
                self.closed.remove(0);
            }
        }
        let opener = tab.opener;
        drop(tab); // closes its WebView
        self.session_dirty = true;

        if idx < self.active {
            self.active -= 1;
        } else if idx == self.active {
            // Back to the page that opened it (e.g. after a login popup), else the neighbour.
            self.active = opener.and_then(|id| self.index_of(id)).unwrap_or(idx.min(self.tabs.len() - 1));
        }
        let active = self.active;
        self.activate(active);
    }

    fn navigate_active(&mut self, url: String) {
        self.suggestions_open = false;
        self.address_edited = false;
        self.page = Page::Web;
        let idx = self.active;
        let tab = &mut self.tabs[idx];
        tab.url = url.clone();
        tab.loading = true;
        match &tab.view {
            Some(view) => view.navigate(&url),
            None => self.ensure_view(idx),
        }
        self.layout_views();
        self.with_active_view(WebView::focus);
        self.session_dirty = true;
        self.refresh_all();
    }

    /// Opens a URL from the bookmarks bar, a suggestion or an internal page.
    fn open_url(&mut self, url: String) {
        let over_page = matches!(self.page, Page::Bookmarks | Page::History);
        if over_page && !self.tabs[self.active].url.is_empty() {
            let idx = self.insert_tab(self.active + 1, url, String::new(), None);
            self.activate(idx);
        } else {
            self.navigate_active(url);
        }
    }

    /// Creates the WebView for a tab that has a URL but none yet.
    fn ensure_view(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        if tab.view.is_some() || tab.creating || tab.url.is_empty() {
            return;
        }
        let id = tab.id;
        self.start_create_view(id, None);
    }

    fn start_create_view(&mut self, id: TabId, new_window: Option<NewWindowRequest>) {
        let (Some(env), Some(hwnd), Some(idx)) = (self.env.clone(), self.hwnd, self.index_of(id)) else {
            if let Some(req) = new_window {
                req.complete_handled();
            }
            return;
        };
        self.tabs[idx].creating = true;
        self.tabs[idx].loading = true;
        let pending = Rc::new(RefCell::new(new_window));
        let pending_cb = pending.clone();
        let private = self.tabs[idx].private;
        let started = webview::create_controller(&env, hwnd, private, move |result| {
            let new_window = pending_cb.borrow_mut().take();
            post(Event::ViewCreated { tab: id, result, new_window });
        });
        if let Err(e) = started {
            log!("CreateCoreWebView2Controller failed: {e}");
            self.tabs[idx].creating = false;
            self.tabs[idx].loading = false;
            if let Some(req) = pending.borrow_mut().take() {
                req.complete_handled();
            }
            self.set_status(&format!("無法建立網頁：{e}"));
        }
    }

    fn on_view_created(
        &mut self,
        id: TabId,
        result: Result<ICoreWebView2Controller, String>,
        new_window: Option<NewWindowRequest>,
    ) {
        let Some(idx) = self.index_of(id) else {
            // Tab closed while its WebView was being created.
            if let Ok(controller) = result {
                unsafe {
                    let _ = controller.Close();
                }
            }
            if let Some(req) = new_window {
                req.complete_handled();
            }
            return;
        };
        self.tabs[idx].creating = false;
        let env = self.env.clone();
        let attached = match (result, env) {
            (Ok(controller), Some(env)) => {
                let blocklist = self.cfg.adblock.then(|| self.blocklist.clone());
                WebView::attach(controller, &env, id, blocklist).map_err(|e| e.message().to_string())
            }
            (Err(e), _) => Err(e),
            (Ok(_), None) => Err("WebView2 環境已關閉".into()),
        };
        match attached {
            Ok(view) => {
                if self.tabs[idx].muted {
                    view.set_muted(true);
                }
                match new_window {
                    // The page's window.open() gets this WebView, so window.opener keeps working.
                    Some(req) => unsafe {
                        let _ = req.args.SetNewWindow(&view.core);
                        let _ = req.deferral.Complete();
                    },
                    None => {
                        let url = self.tabs[idx].url.clone();
                        view.navigate(&url);
                    }
                }
                let tab = &mut self.tabs[idx];
                tab.view = Some(view);
                tab.suspended = false;
                self.apply_zoom(idx);
                self.layout_views();
                // Focus the page unless the user has started typing a new address meanwhile.
                if idx == self.active && self.page == Page::Web && !self.address_edited {
                    self.with_active_view(WebView::focus);
                }
                if !self.extensions_synced {
                    self.sync_extensions(false);
                }
            }
            Err(e) => {
                log!("tab {id}: cannot create WebView: {e}");
                self.tabs[idx].loading = false;
                if let Some(req) = new_window {
                    req.complete_handled();
                }
                self.set_status(&format!("無法建立網頁：{e}"));
            }
        }
        self.refresh_all();
        self.apply_policy(None);
    }

    fn web_shown(&self) -> bool {
        self.page == Page::Web && !self.suggestions_open && !self.minimized
    }

    /// Shows the active tab's WebView over the page area (or hides it when a Slint page or the
    /// suggestion list needs that space), hides the others, and positions the DevTools view.
    fn layout_views(&mut self) {
        let show = self.web_shown();
        let bounds = self.web_view_bounds();
        let mut active_controller = None;
        for (i, tab) in self.tabs.iter().enumerate() {
            let Some(view) = &tab.view else { continue };
            if i == self.active {
                view.set_bounds(bounds);
                view.set_visible(show);
                active_controller = Some(view.controller.clone());
            } else {
                view.set_visible(false);
            }
        }
        win::set_active_controller(active_controller);

        if let Some(view) = &self.devtools_view {
            view.set_bounds(self.devtools_bounds());
            view.set_visible(show);
        }
    }

    /// Hides every piece of chrome and gives the page the whole window, or puts it all back.
    fn apply_fullscreen(&mut self) {
        let on = self.page_fullscreen || self.user_fullscreen;
        if on == self.ui.get_fullscreen() {
            return;
        }
        self.ui.set_fullscreen(on);
        self.ui.window().set_fullscreen(on);
        // Esc is only taken from the page while *we* are the ones holding full screen; a page's
        // own full screen leaves on Esc by itself.
        webview::set_browser_fullscreen(self.user_fullscreen);
        // The chrome is gone, so `web-slot` has grown. Slint reports that through
        // `content-geometry-changed`, but move the native views now so there is no blank frame.
        self.layout_views();
        if let Some(hwnd) = self.hwnd {
            win::repaint_all(hwnd);
        }
    }

    fn rect_from(&self, x: f32, y: f32, w: f32, h: f32) -> RECT {
        let scale = self.ui.window().scale_factor();
        let (x, y, w, h) = (x * scale, y * scale, w * scale, h * scale);
        RECT { left: x.round() as i32, top: y.round() as i32, right: (x + w).round() as i32, bottom: (y + h).round() as i32 }
    }

    fn web_view_bounds(&self) -> RECT {
        self.rect_from(
            self.ui.get_web_view_x(),
            self.ui.get_web_view_y(),
            self.ui.get_web_view_width(),
            self.ui.get_web_view_height(),
        )
    }

    fn devtools_bounds(&self) -> RECT {
        self.rect_from(
            self.ui.get_devtools_x(),
            self.ui.get_devtools_y(),
            self.ui.get_devtools_width(),
            self.ui.get_devtools_height(),
        )
    }

    fn discard(&mut self, id: TabId) {
        let Some(idx) = self.index_of(id) else { return };
        let tab = &mut self.tabs[idx];
        if idx == self.active || tab.creating || tab.view.is_none() {
            return;
        }
        log!("tab {id}: discarded ({} MB)", tab.bytes >> 20);
        tab.drop_view();
    }

    fn suspend(&mut self, id: TabId) {
        let Some(idx) = self.index_of(id) else { return };
        let tab = &mut self.tabs[idx];
        if idx == self.active || tab.suspended {
            return;
        }
        if let Some(view) = &tab.view {
            view.set_visible(false);
            view.try_suspend(id);
            tab.suspended = true;
        }
    }

    /// Puts the tab's page at the zoom its site was last left at.
    fn apply_zoom(&self, idx: usize) {
        let Some(view) = self.tabs.get(idx).and_then(|t| t.view.as_ref()) else { return };
        let target = url_input::host_of(&self.tabs[idx].url).map_or(1.0, |host| self.zoom.get(&host));
        if (view.zoom() - target).abs() > 0.001 {
            view.set_zoom(target);
        }
    }

    /// Makes `factor` the zoom of the tab's site: remembered, and the site's other tabs follow,
    /// like Chrome.
    fn set_site_zoom(&mut self, idx: usize, factor: f64) {
        let tab = &self.tabs[idx];
        // WebView2 keeps a zoom we set across the tab's navigations, but drops one made with
        // Ctrl+± or the wheel on the next page and reports that drop as a change, which would
        // forget the site's zoom. Setting it ourselves makes it stick. Our own sets raise no
        // ZoomFactorChanged, so this does not come back here.
        if let Some(view) = &tab.view {
            view.set_zoom(factor);
        }
        // A private tab follows the sites' zoom but leaves no record of its visits.
        let Some(host) = url_input::host_of(&tab.url).filter(|_| !tab.private) else { return };
        self.zoom.set(&host, factor);
        for i in 0..self.tabs.len() {
            if i != idx && url_input::host_of(&self.tabs[i].url).as_deref() == Some(host.as_str()) {
                self.apply_zoom(i);
            }
        }
    }

    fn on_renderer_frames(&mut self, renderers: Vec<(u32, Vec<u32>)>) {
        let frames: Vec<Option<u32>> = self.tabs.iter().map(|t| t.view.as_ref().and_then(WebView::frame_id)).collect();
        for (tab, bytes) in self.tabs.iter_mut().zip(memory::per_tab(&frames, &renderers, memory::private_memory)) {
            tab.bytes = bytes;
        }
    }

    fn apply_policy(&mut self, memory: Option<MemoryState>) {
        let snapshots: Vec<TabSnapshot> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabSnapshot {
                id: t.id,
                residency: t.residency(),
                active: i == self.active,
                pinned: t.pinned,
                unsaved: t.unsaved,
                // Muting is how the user says the sound may go.
                audible: t.audible && !t.muted,
                last_active_ms: t.last_active_ms,
                bytes: t.bytes,
            })
            .collect();
        let actions = tabs::plan(&snapshots, self.now_ms(), memory, &Policy::from_config(&self.cfg));
        if actions.is_empty() {
            return;
        }
        for action in actions {
            match action {
                Action::Suspend(id) => self.suspend(id),
                Action::Discard(id) => self.discard(id),
            }
        }
        self.refresh_tabs();
        self.refresh_status();
    }

    // -----------------------------------------------------------------------------------------
    // WebView2 events

    fn handle_engine(&mut self, id: TabId, event: EngineEvent) {
        let Some(idx) = self.index_of(id) else {
            match event {
                EngineEvent::NewWindow(req) => req.complete_handled(),
                // The docked DevTools view is not a tab (id 0), but its keys still drive the
                // browser — F12 there must close it.
                EngineEvent::Shortcut(s) => self.run_shortcut(s),
                _ => {}
            }
            return;
        };
        let is_active = idx == self.active;
        match event {
            EngineEvent::TitleChanged(title) => {
                let tab = &mut self.tabs[idx];
                tab.title = title;
                if !tab.private {
                    self.history.update_title(&tab.url, &tab.title);
                }
                self.session_dirty = true;
                self.refresh_tabs();
                if is_active {
                    self.refresh_title();
                }
            }
            EngineEvent::SourceChanged(url) => {
                let tab = &mut self.tabs[idx];
                if tab.url != url {
                    tab.url = url;
                    tab.favicon = Image::default();
                    self.session_dirty = true;
                    // Show a cached icon immediately; a fresh one may arrive via FaviconChanged.
                    self.set_tab_favicon_from_cache(idx);
                }
                // The page's zoom carries across navigations, so each site gets its own here. On
                // every change, not only when the URL differs: the omnibox and MCP set the tab's
                // URL before the page loads.
                self.apply_zoom(idx);
                let tab = &mut self.tabs[idx];
                if tab.recorded_url != tab.url && !tab.private {
                    tab.recorded_url = tab.url.clone();
                    self.history.record(&tab.url, &tab.title, storage::now_secs());
                }
                self.refresh_tabs();
                if is_active {
                    self.sync_address();
                    self.refresh_toolbar();
                }
            }
            EngineEvent::NavigationStarting => {
                self.tabs[idx].loading = true;
                // The new document starts without the old one's typing.
                self.tabs[idx].unsaved = false;
                self.refresh_tabs();
                if is_active {
                    self.refresh_toolbar();
                }
            }
            EngineEvent::NavigationCompleted => {
                self.tabs[idx].loading = false;
                self.refresh_tabs();
                if is_active {
                    self.refresh_toolbar();
                }
            }
            EngineEvent::HistoryChanged { can_back, can_forward } => {
                self.tabs[idx].can_back = can_back;
                self.tabs[idx].can_forward = can_forward;
                if is_active {
                    self.refresh_toolbar();
                }
            }
            EngineEvent::NewWindow(req) => self.on_new_window(idx, req),
            EngineEvent::CloseRequested => self.close_tab(idx),
            EngineEvent::RendererGone => {
                self.tabs[idx].drop_view();
                if is_active && self.page == Page::Web {
                    self.set_status("分頁的網頁程序結束了，已重新載入");
                    self.ensure_view(idx);
                }
                self.refresh_all();
            }
            EngineEvent::BrowserGone => self.on_browser_gone(),
            EngineEvent::Shortcut(s) => self.run_shortcut(s),
            EngineEvent::SuspendFinished(ok) => {
                if !ok {
                    // Could not freeze (e.g. audio playing); at least ask it to trim memory.
                    if let Some(view) = &self.tabs[idx].view {
                        view.set_low_memory(true);
                    }
                }
                self.refresh_tabs();
            }
            EngineEvent::UnsavedInput(unsaved) => {
                // Explains in the log why a tab stays loaded past its discard time.
                if self.tabs[idx].unsaved != unsaved {
                    log!("tab {id}: {}", if unsaved { "holds unsent input" } else { "unsent input gone" });
                }
                self.tabs[idx].unsaved = unsaved;
            }
            EngineEvent::Audible(playing) => {
                self.tabs[idx].audible = playing;
                self.refresh_tabs();
            }
            EngineEvent::Zoom(factor) => {
                self.set_site_zoom(idx, factor);
            }
            EngineEvent::FullScreen(on) => {
                // A background tab's video must not take over the window.
                if idx == self.active {
                    self.page_fullscreen = on;
                    self.apply_fullscreen();
                }
            }
            EngineEvent::Favicon(bytes) => {
                // The new page has no icon: drop the previous page's from the tab, cache nothing.
                if bytes.is_empty() {
                    self.tabs[idx].favicon = Image::default();
                    self.refresh_tabs();
                    return;
                }
                let url = self.tabs[idx].url.clone();
                // The disk cache would record a private visit, so a private tab's icon lives
                // only on that tab.
                if self.tabs[idx].private {
                    if let Some(image) = favicon::decode_png(&bytes) {
                        self.tabs[idx].favicon = image;
                        self.refresh_tabs();
                    }
                    return;
                }
                if let Some(image) = self.favicons.store(&url, &bytes) {
                    // Apply to every tab on the same host.
                    let host = url_input::host_of(&url);
                    for tab in &mut self.tabs {
                        if url_input::host_of(&tab.url) == host {
                            tab.favicon = image.clone();
                        }
                    }
                    self.refresh_tabs();
                    // The bookmarks bar is always on screen, so it cannot wait for a page switch.
                    if self.bookmarks.items().iter().any(|b| url_input::host_of(&b.url) == host) {
                        self.refresh_bookmarks();
                    }
                    if matches!(self.page, Page::History | Page::NewTab) {
                        self.refresh_history();
                        self.refresh_top_sites();
                    }
                }
            }
        }
    }

    fn on_new_window(&mut self, opener_idx: usize, req: NewWindowRequest) {
        let opener = self.tabs[opener_idx].id;
        let lazy_url = !req.uri.is_empty() && req.uri != "about:blank";
        if req.background && lazy_url {
            // Ctrl/middle click: a discarded tab that loads when first opened. Costs no memory.
            req.complete_handled();
            self.insert_tab(opener_idx + 1, req.uri, String::new(), Some(opener));
            self.refresh_all();
            return;
        }
        // window.open() without a URL: the opener fills the page in; show it as a web page anyway.
        let url = if req.uri.is_empty() { "about:blank".to_string() } else { req.uri.clone() };
        let idx = self.insert_tab(opener_idx + 1, url, String::new(), Some(opener));
        let id = self.tabs[idx].id;
        self.start_create_view(id, Some(req));
        self.activate(idx);
    }

    fn on_browser_gone(&mut self) {
        if self.env.is_none() {
            return; // already restarting
        }
        log!("WebView2 browser process exited; restarting");
        for tab in &mut self.tabs {
            tab.drop_view();
            tab.creating = false;
        }
        win::set_active_controller(None);
        self.env = None;
        self.extensions_synced = false;
        self.set_status("WebView2 已重新啟動");
        self.create_environment();
    }

    // -----------------------------------------------------------------------------------------
    // Address bar and pages

    fn on_address_edited(&mut self, text: String) {
        self.address_edited = true;
        if text.trim().is_empty() {
            self.set_suggestions_open(false);
            return;
        }
        // Private tabs stay out, as in Chrome, where they live in a window of their own.
        let open: Vec<(TabId, &str, &str)> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| *i != self.active && !t.private && !t.url.is_empty())
            .map(|(_, t)| (t.id, t.title.as_str(), t.url.as_str()))
            .collect();
        let items: Vec<SuggestionData> =
            storage::suggestions(&text, &open, &self.bookmarks, &self.history, &self.cfg.search_url, 8)
                .into_iter()
                .map(|s| SuggestionData {
                    title: s.title.into(),
                    url: s.url.into(),
                    kind: match s.kind {
                        SuggestionKind::Search => "search",
                        SuggestionKind::Url => "url",
                        SuggestionKind::Bookmark => "bookmark",
                        SuggestionKind::History => "history",
                        SuggestionKind::Tab(_) => "tab",
                    }
                    .into(),
                    tab_id: if let SuggestionKind::Tab(id) = s.kind { id as i32 } else { -1 },
                })
                .collect();
        self.models.suggestions.set_vec(items);
        self.ui.set_selected_suggestion(-1);
        self.set_suggestions_open(true);
    }

    fn set_suggestions_open(&mut self, open: bool) {
        if self.suggestions_open != open {
            self.suggestions_open = open;
            self.ui.set_suggestions_open(open);
            self.layout_views();
        }
    }

    fn sync_address(&mut self) {
        if self.address_focused && self.address_edited {
            return; // don't overwrite what the user is typing
        }
        self.address_edited = false;
        let url = self.tabs.get(self.active).map(|t| t.url.clone()).unwrap_or_default();
        let kind = if url.starts_with("https://") {
            "secure"
        } else if url.starts_with("http://") {
            "insecure"
        } else {
            ""
        };
        self.ui.set_site_kind(kind.into());
        self.ui.set_display_address(url_input::display(&url).into());
        self.ui.set_page_url(url.clone().into());
        self.ui.set_address(url.into());
    }

    fn show_page(&mut self, page: Page) {
        // Clicking the button of the page that is already open returns to the web page.
        if page == self.page && page != Page::Web {
            let active = self.active;
            self.activate(active);
            return;
        }
        self.page = page;
        self.suggestions_open = false;
        match page {
            Page::Bookmarks => self.refresh_bookmarks(),
            Page::History => {
                self.history_query.clear();
                self.refresh_history();
            }
            Page::Extensions => {
                self.ui.set_extensions_message(SharedString::new());
                self.request_extension_list();
            }
            Page::Settings => self.ui.set_settings(self.settings_data()),
            Page::Import => {
                if self.import_profiles.is_empty() {
                    self.refresh_import();
                }
            }
            Page::NewTab => self.refresh_top_sites(),
            Page::Web => {}
        }
        self.layout_views();
        self.refresh_all();
    }

    fn settings_data(&self) -> SettingsData {
        SettingsData {
            max_live_tabs: self.cfg.max_live_tabs as i32,
            suspend_after_secs: self.cfg.suspend_after_secs as i32,
            discard_after_mins: self.cfg.discard_after_mins as i32,
            memory_budget_mb: self.cfg.memory_budget_mb as i32,
            adblock: self.cfg.adblock,
            disable_gpu: self.cfg.disable_gpu,
            restore_session: self.cfg.restore_session,
            show_bookmarks_bar: self.cfg.show_bookmarks_bar,
            search_url: self.cfg.search_url.clone().into(),
            devtools_dock_right: self.cfg.devtools_dock_right,
            mcp_enabled: self.cfg.mcp_enabled,
            mcp_host: self.cfg.mcp_host.clone().into(),
            mcp_port: self.cfg.mcp_port as i32,
        }
    }

    fn save_settings(&mut self, s: SettingsData) {
        let old = self.cfg.clone();
        self.cfg = Config {
            max_live_tabs: s.max_live_tabs.max(1) as usize,
            suspend_after_secs: s.suspend_after_secs.max(0) as u64,
            discard_after_mins: s.discard_after_mins.max(0) as u64,
            memory_budget_mb: s.memory_budget_mb.max(0) as u64,
            adblock: s.adblock,
            disable_gpu: s.disable_gpu,
            restore_session: s.restore_session,
            show_bookmarks_bar: s.show_bookmarks_bar,
            search_url: s.search_url.to_string(),
            devtools_dock_right: s.devtools_dock_right,
            mcp_enabled: s.mcp_enabled,
            mcp_host: s.mcp_host.to_string(),
            mcp_port: s.mcp_port.clamp(0, u16::MAX as i32) as u16,
            ..old.clone()
        }
        .sanitized();
        let saved = self.cfg.save(&self.paths.config());
        self.ui.set_settings(self.settings_data());
        self.ui.set_devtools_right(self.cfg.devtools_dock_right);
        self.layout_views();
        let needs_restart = old.disable_gpu != self.cfg.disable_gpu
            || old.adblock != self.cfg.adblock
            || old.mcp_enabled != self.cfg.mcp_enabled
            || old.mcp_host != self.cfg.mcp_host
            || old.mcp_port != self.cfg.mcp_port;
        self.set_status(match (saved.is_ok(), needs_restart) {
            (false, _) => "設定無法寫入檔案",
            (true, true) => "設定已儲存；GPU／廣告封鎖／MCP 的變更在重新啟動後生效",
            (true, false) => "設定已儲存",
        });
        self.apply_policy(None);
    }

    // -----------------------------------------------------------------------------------------
    // Extensions

    fn any_profile(&self) -> Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Profile7> {
        self.tabs.iter().filter_map(|t| t.view.as_ref()).find_map(WebView::profile)
    }

    /// Installs extension folders that were not installed yet (or all of them when `force`).
    fn sync_extensions(&mut self, force: bool) {
        let Some(profile) = self.any_profile() else {
            self.ui.set_extensions_message("請先開啟任一網頁分頁，才能管理擴充功能。".into());
            return;
        };
        self.extensions_synced = true;
        let Ok(entries) = std::fs::read_dir(self.paths.extensions_dir()) else { return };
        let mut folders: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.join("manifest.json").is_file())
            .filter_map(|p| Some((p.file_name()?.to_string_lossy().into_owned(), p)))
            .collect();
        folders.sort();
        let mut started = 0;
        for (name, path) in folders {
            if !force && self.installed_extensions.contains_key(&name) {
                continue;
            }
            started += 1;
            webview::add_extension(&profile, &path, move |result| {
                post(Event::ExtensionInstalled { folder: name, result });
            });
        }
        if force {
            self.ui.set_extensions_message(
                if started == 0 { "擴充功能資料夾裡沒有可安裝的擴充功能（需要含 manifest.json 的資料夾）。" } else { "安裝中…" }.into(),
            );
            if started == 0 {
                self.request_extension_list();
            }
        }
    }

    fn on_extension_installed(&mut self, folder: String, result: Result<ExtensionInfo, String>) {
        match result {
            Ok(info) => {
                log!("extension installed: {} ({})", info.name, info.id);
                self.ui.set_extensions_message(format!("已安裝「{}」，重新整理網頁後生效。", info.name).into());
                self.installed_extensions.insert(folder, info.id);
            }
            Err(e) => {
                log!("extension {folder} failed: {e}");
                self.ui.set_extensions_message(format!("「{folder}」安裝失敗：{e}").into());
                // Remember the failure too so it is not retried on every start; "重新載入" retries.
                self.installed_extensions.insert(folder, String::new());
            }
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.installed_extensions) {
            let _ = crate::paths::write_atomic(&self.paths.root.join("extensions.json"), &bytes);
        }
        self.request_extension_list();
    }

    fn request_extension_list(&mut self) {
        if self.page != Page::Extensions {
            return;
        }
        match self.any_profile() {
            Some(profile) => webview::list_extensions(&profile, |result| post(Event::ExtensionsListed(result))),
            None => {
                self.models.extensions.set_vec(Vec::new());
                self.ui.set_extensions_message("請先開啟任一網頁分頁，才能管理擴充功能。".into());
            }
        }
    }

    fn on_extensions_listed(&mut self, result: Result<Vec<ExtensionInfo>, String>) {
        match result {
            Ok(list) => {
                if list.is_empty() && self.ui.get_extensions_message().is_empty() {
                    self.ui.set_extensions_message("目前沒有安裝擴充功能。".into());
                }
                self.models.extensions.set_vec(
                    list.into_iter()
                        .map(|e| ExtensionData { id: e.id.into(), name: e.name.into(), enabled: e.enabled })
                        .collect::<Vec<_>>(),
                );
            }
            Err(e) => self.ui.set_extensions_message(format!("無法讀取擴充功能：{e}").into()),
        }
    }

    /// Downloads an extension from the Chrome Web Store and installs what comes out of it.
    fn install_from_store(&mut self, input: &str) {
        if self.extension_installing {
            return;
        }
        let Some(id) = crx::parse_store_url(input) else {
            self.ui.set_extensions_message(
                "看不出擴充功能 ID。請貼上商店頁面的網址，例如 https://chromewebstore.google.com/detail/名稱/擴充功能ID。"
                    .into(),
            );
            return;
        };
        if self.any_profile().is_none() {
            self.ui.set_extensions_message("請先開啟任一網頁分頁，才能安裝擴充功能。".into());
            return;
        }
        self.extension_installing = true;
        self.ui.set_extension_installing(true);
        self.ui.set_extensions_message(format!("正在下載 {id}…").into());

        let dir = self.paths.extensions_dir();
        std::thread::spawn(move || {
            let result = crx::install(&id, &dir);
            // Events carry COM pointers, so one is only ever built on the UI thread.
            let _ = slint::invoke_from_event_loop(move || post(Event::ExtensionDownloaded(result)));
        });
    }

    fn on_extension_downloaded(&mut self, result: Result<String, String>) {
        self.extension_installing = false;
        self.ui.set_extension_installing(false);
        let folder = match result {
            Ok(folder) => folder,
            Err(e) => {
                log!("store install failed: {e}");
                self.ui.set_extensions_message(format!("下載失敗：{e}").into());
                return;
            }
        };
        let path = self.paths.extensions_dir().join(&folder);
        let name = crx::manifest_name(&path).unwrap_or_else(|| folder.clone());
        let Some(profile) = self.any_profile() else {
            self.ui.set_extensions_message(format!("已下載「{name}」，但需要開啟任一網頁分頁才能安裝。").into());
            return;
        };
        log!("downloaded extension {folder} ({name})");
        self.ui.set_extensions_message(format!("已下載「{name}」，正在安裝…").into());
        // Dropping the bookkeeping entry means a re-download reinstalls rather than being skipped.
        self.installed_extensions.remove(&folder);
        webview::add_extension(&profile, &path, move |result| {
            post(Event::ExtensionInstalled { folder, result });
        });
    }

    fn change_extension(&mut self, id: String, change: ExtensionChange) {
        match self.any_profile() {
            Some(profile) => {
                webview::change_extension(&profile, id, change, |result| post(Event::ExtensionChanged(result)))
            }
            None => self.ui.set_extensions_message("請先開啟任一網頁分頁，才能管理擴充功能。".into()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Periodic work

    fn tick(&mut self) {
        let minimized = self.ui.window().is_minimized();
        if minimized != self.minimized {
            self.minimized = minimized;
            // A minimized browser lets the visible page trim its memory too.
            if let Some(view) = self.tabs.get(self.active).and_then(|t| t.view.as_ref()) {
                view.set_low_memory(minimized);
            }
            self.layout_views();
            if !minimized {
                // The software renderer can come back from minimize with stale chrome (the toolbar
                // area goes blank), so force a full repaint.
                self.ui.window().request_redraw();
                if let Some(hwnd) = self.hwnd {
                    win::repaint_all(hwnd);
                    win::restore_page_focus(hwnd);
                }
            }
        }

        let maximized = self.ui.window().is_maximized();
        if maximized != self.ui.get_window_maximized() {
            self.ui.set_window_maximized(maximized);
        }

        self.usage = memory::browser_usage();
        let sys = platform::system_memory();
        self.system_memory_free = sys.free_bytes;
        self.apply_policy(Some(MemoryState { browser_bytes: self.usage.bytes, system_free_bytes: sys.free_bytes }));
        // Per-tab sizes for the next round's policy: the answer comes back asynchronously.
        if let Some(env) = &self.env {
            webview::renderer_frames(env, |renderers| post(Event::RendererFrames(renderers)));
        }
        // Every message is a one-off notice ("bookmark added", "private tab: …"); left up, it
        // goes on describing a tab or an action that is long gone.
        if !self.status_message.is_empty() && self.now_ms().saturating_sub(self.status_message_ms) > 8_000 {
            self.status_message.clear();
        }
        self.refresh_status();

        let now = self.now_ms();
        if now.saturating_sub(self.last_save_ms) > 30_000 {
            self.bookmarks.save_if_dirty();
            self.history.save_if_dirty();
            self.zoom.save_if_dirty();
            if self.session_dirty {
                self.save_session();
            }
            self.last_save_ms = now;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Pushing state to the UI

    fn refresh_all(&mut self) {
        self.ui.set_page(self.page);
        self.ui.set_suggestions_open(self.suggestions_open);
        self.refresh_tabs();
        self.refresh_toolbar();
        self.refresh_title();
        self.sync_address();
        self.refresh_status();
    }

    fn refresh_tabs(&self) {
        let data: Vec<TabData> = self
            .tabs
            .iter()
            .map(|t| TabData {
                title: t.title.clone().into(),
                url: t.url.clone().into(),
                loading: t.loading && (t.view.is_some() || t.creating),
                discarded: t.residency() == Residency::Discarded && !t.url.is_empty(),
                suspended: t.suspended,
                pinned: t.pinned,
                favicon: t.favicon.clone(),
                private: t.private,
                audible: t.audible,
                muted: t.muted,
            })
            .collect();
        self.models.tabs.set_vec(data);
        self.ui.set_active_tab(self.active as i32);
        self.ui.set_pinned_count(self.tabs.iter().filter(|t| t.pinned).count() as i32);
    }

    fn refresh_toolbar(&self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let web = self.page == Page::Web;
        self.ui.set_can_go_back(web && tab.view.is_some() && tab.can_back);
        self.ui.set_can_go_forward(web && tab.view.is_some() && tab.can_forward);
        self.ui.set_loading(web && tab.loading);
        self.ui.set_bookmarked(web && self.bookmarks.contains(&tab.url));
    }

    fn refresh_title(&self) {
        let title = self.tabs.get(self.active).map(|t| t.title.as_str()).unwrap_or("");
        let title = if title.is_empty() { "LiteBrowser".to_string() } else { format!("{title} - LiteBrowser") };
        self.ui.set_window_title(title.into());
    }

    fn refresh_status(&self) {
        let resident = self.tabs.iter().filter(|t| t.residency() != Residency::Discarded).count();
        let mut status = format!("分頁 {}（保留網頁 {}）", self.tabs.len(), resident);
        let blocked = webview::blocked_requests();
        if blocked > 0 {
            status.push_str(&format!(" · 已封鎖 {blocked} 個請求"));
        }
        if !self.status_message.is_empty() {
            status.push_str(" · ");
            status.push_str(&self.status_message);
        }
        self.ui.set_status_text(status.into());

        if self.usage.bytes > 0 {
            let mb = self.usage.bytes / (1024 * 1024);
            let mut text = format!("記憶體 {mb} MB / {} MB（{} 個程序）", self.cfg.memory_budget_mb, self.usage.processes);
            if self.system_memory_free > 0 {
                text.push_str(&format!("　系統剩餘 {} MB", self.system_memory_free / (1024 * 1024)));
            }
            self.ui.set_memory_text(text.into());
            self.ui.set_memory_over(mb > self.cfg.memory_budget_mb);
        }
    }

    fn refresh_bookmarks(&mut self) {
        let entries: Vec<(String, String, String)> =
            self.bookmarks.items().iter().map(|b| (b.title.clone(), b.url.clone(), b.folder.clone())).collect();
        let items: Vec<LinkData> = entries
            .into_iter()
            .map(|(title, url, folder)| LinkData {
                title: title.into(),
                detail: folder.into(),
                favicon: self.favicons.get(&url).unwrap_or_default(),
                url: url.into(),
            })
            .collect();
        let bar: Vec<BarItemData> = self
            .bar_entries()
            .into_iter()
            .map(|entry| match entry {
                FolderEntry::Bookmark(i) => BarItemData {
                    title: items[i].title.clone(),
                    url: items[i].url.clone(),
                    favicon: items[i].favicon.clone(),
                    index: i as i32,
                    folder: false,
                },
                FolderEntry::Folder(path, name) => {
                    BarItemData { title: name.into(), url: path.into(), favicon: Image::default(), index: -1, folder: true }
                }
            })
            .collect();
        self.models.bookmarks.set_vec(items);
        self.models.bar.set_vec(bar);
        let other = self.bookmarks.folder_entries("").iter().any(|e| matches!(e, FolderEntry::Folder(p, _) if p == OTHER_FOLDER));
        self.ui.set_other_folder(if other { OTHER_FOLDER.into() } else { SharedString::new() });
    }

    /// The bookmarks bar's chips in order: the top level, minus 其他書籤, which has its own place
    /// at the bar's right end.
    fn bar_entries(&self) -> Vec<FolderEntry> {
        let mut entries = self.bookmarks.folder_entries("");
        entries.retain(|e| !matches!(e, FolderEntry::Folder(path, _) if path == OTHER_FOLDER));
        entries
    }

    fn refresh_history(&mut self) {
        let now = storage::now_secs();
        let entries: Vec<(String, String, u64)> = if self.history_query.trim().is_empty() {
            self.history.items().iter().take(300).map(|h| (h.url.clone(), h.title.clone(), h.last_visit)).collect()
        } else {
            self.history.search(&self.history_query, 300).iter().map(|h| (h.url.clone(), h.title.clone(), h.last_visit)).collect()
        };
        let items: Vec<LinkData> = entries
            .into_iter()
            .map(|(url, title, last_visit)| LinkData {
                title: if title.is_empty() { url.clone().into() } else { title.into() },
                detail: storage::relative_time(last_visit, now).into(),
                favicon: self.favicons.get(&url).unwrap_or_default(),
                url: url.into(),
            })
            .collect();
        self.models.history.set_vec(items);
    }

    fn refresh_top_sites(&mut self) {
        let mut top: Vec<(String, String)> =
            self.history.top_sites(8).into_iter().map(|h| (h.url.clone(), h.title.clone())).collect();
        // Until there is any history, the new tab page's tiles show the first bookmarks instead.
        if top.is_empty() {
            top = self.bookmarks.items().iter().take(8).map(|b| (b.url.clone(), b.title.clone())).collect();
        }
        let items: Vec<LinkData> = top
            .into_iter()
            .map(|(url, title)| LinkData {
                title: if title.is_empty() { url.clone().into() } else { title.into() },
                detail: SharedString::new(),
                favicon: self.favicons.get(&url).unwrap_or_default(),
                url: url.into(),
            })
            .collect();
        self.models.top_sites.set_vec(items);
    }

    // -----------------------------------------------------------------------------------------
    // Tab management: pinning, duplicate, context menu, reordering

    /// Keeps pinned tabs as the first entries (stable within each group), tracking the active tab.
    fn reorder_pinned_first(&mut self) {
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        self.tabs.sort_by_key(|t| !t.pinned); // false (pinned) sorts before true
        if let Some(id) = active_id {
            self.active = self.index_of(id).unwrap_or(0);
        }
    }

    fn set_tab_favicon_from_cache(&mut self, idx: usize) {
        if let Some(tab) = self.tabs.get(idx) {
            let url = tab.url.clone();
            if let Some(image) = self.favicons.get(&url) {
                self.tabs[idx].favicon = image;
            }
        }
    }

    fn tab_context_menu(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        let pinned = tab.pinned;
        let (id, releasable) = (tab.id, idx != self.active && tab.view.is_some() && !tab.creating);
        // The size the measurement put on this tab, so the user can see what releasing it buys.
        let release = match (tab.bytes >> 20, tab.unsaved) {
            (0, false) => "釋放分頁".to_string(),
            (0, true) => "釋放分頁（有未送出的輸入）".to_string(),
            (mb, false) => format!("釋放分頁（目前 {mb} MB）"),
            (mb, true) => format!("釋放分頁（目前 {mb} MB，有未送出的輸入）"),
        };
        let menu = vec![
            MenuItem::entry(1, if pinned { "取消釘選" } else { "釘選分頁" }),
            MenuItem::entry(2, "複製分頁"),
            MenuItem::entry(10, "開新無痕分頁"),
            MenuItem::entry(3, "重新載入"),
            MenuItem::entry(12, if tab.muted { "取消分頁靜音" } else { "將分頁靜音" }),
            if releasable { MenuItem::entry(11, release) } else { MenuItem::disabled(release) },
            MenuItem::Separator,
            MenuItem::entry(4, "往左移"),
            MenuItem::entry(5, "往右移"),
            MenuItem::Separator,
            MenuItem::entry(6, "關閉分頁"),
            if self.tabs.len() > 1 { MenuItem::entry(7, "關閉其他分頁") } else { MenuItem::disabled("關閉其他分頁") },
            MenuItem::entry(8, "關閉右側分頁"),
            if self.closed.is_empty() { MenuItem::disabled("重新開啟關閉的分頁") } else { MenuItem::entry(9, "重新開啟關閉的分頁") },
        ];
        match platform::popup_menu(&menu) {
            1 => self.toggle_pin(idx),
            2 => self.duplicate_tab(idx),
            3 => {
                if let Some(view) = self.tabs.get(idx).and_then(|t| t.view.as_ref()) {
                    view.reload();
                } else {
                    self.ensure_view(idx);
                }
            }
            4 => self.move_tab(idx, idx.wrapping_sub(1)),
            5 => self.move_tab(idx, idx + 1),
            6 => self.close_tab(idx),
            7 => self.close_other_tabs(idx),
            8 => self.close_tabs_to_right(idx),
            9 => self.run_shortcut(Shortcut::ReopenClosedTab),
            10 => self.run_shortcut(Shortcut::NewPrivateTab),
            11 => {
                self.discard(id);
                self.refresh_tabs();
                self.refresh_status();
            }
            12 => self.toggle_mute(idx),
            _ => {}
        }
    }

    fn toggle_mute(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        tab.muted = !tab.muted;
        if let Some(view) = &tab.view {
            view.set_muted(tab.muted);
        }
        self.refresh_tabs();
    }

    fn bookmark_menu(&mut self, idx: usize) {
        let Some(url) = self.bookmarks.items().get(idx).map(|b| b.url.clone()) else { return };
        let menu = vec![
            MenuItem::entry(1, "開啟"),
            MenuItem::entry(2, "在新分頁中開啟"),
            MenuItem::Separator,
            MenuItem::entry(3, "編輯…"),
            MenuItem::entry(4, "刪除"),
        ];
        match platform::popup_menu(&menu) {
            1 => self.open_url(url),
            2 => self.handle_ui(UiEvent::OpenUrlNewTab(url)),
            3 => {
                // Reuses the bookmarks page's inline editor; show_page on the open page would
                // toggle back to the web page instead.
                if self.page != Page::Bookmarks {
                    self.show_page(Page::Bookmarks);
                }
                self.ui.set_editing_bookmark(idx as i32);
            }
            4 => self.handle_ui(UiEvent::RemoveBookmark(idx)),
            _ => {}
        }
    }

    /// A bookmarks-bar folder drops down as a native menu (a Slint one would sink under the
    /// WebView), its subfolders as submenus.
    fn bookmark_folder_menu(&mut self, path: &str, x: f32, y: f32) {
        let entries = self.bookmarks.folder_entries(path);
        self.open_bookmark_menu(entries, x, y, false);
    }

    /// The » button lists the bar's chips from the first one that did not fit.
    fn bookmark_overflow_menu(&mut self, first: usize, x: f32, y: f32) {
        let entries = self.bar_entries().into_iter().skip(first).collect();
        self.open_bookmark_menu(entries, x, y, true);
    }

    /// Shows a menu of bookmark entries hanging from (x, y), logical pixels, and opens the pick.
    fn open_bookmark_menu(&mut self, entries: Vec<FolderEntry>, x: f32, y: f32, right_aligned: bool) {
        let scale = self.ui.window().scale_factor();
        // Menu icons are small icons: 16 px at 100 %.
        let px = (16.0 * scale).round() as u32;
        let mut icons = MenuIcons {
            favicons: &mut self.favicons,
            px,
            folder: favicon::menu_glyph(FOLDER_GLYPH, px),
            globe: favicon::menu_glyph(GLOBE_GLYPH, px),
        };
        let menu = bookmark_menu_items(&self.bookmarks, &mut icons, entries);
        let chosen = platform::popup_menu_below(&menu, (x * scale).round() as i32, (y * scale).round() as i32, right_aligned);
        if let Some(b) = (chosen as usize).checked_sub(1).and_then(|i| self.bookmarks.items().get(i)) {
            let url = b.url.clone();
            self.open_url(url);
        }
    }

    fn toggle_pin(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        tab.pinned = !tab.pinned;
        self.reorder_pinned_first();
        self.session_dirty = true;
        let active = self.active;
        self.activate(active);
    }

    fn duplicate_tab(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        let (url, title) = (tab.url.clone(), tab.title.clone());
        if url.is_empty() {
            return;
        }
        let private = tab.private;
        let at = idx + 1;
        let new_idx = self.insert_tab(at, url, title, None);
        self.tabs[new_idx].private = private;
        self.activate(new_idx);
    }

    fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        // Don't let a move cross the pinned/unpinned boundary.
        if self.tabs[from].pinned != self.tabs[to].pinned {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self.index_of_active_after_move(from, to);
        self.session_dirty = true;
        self.refresh_tabs();
    }

    fn index_of_active_after_move(&self, from: usize, to: usize) -> usize {
        // Recompute the active index after moving an element from `from` to `to`.
        let a = self.active;
        if a == from {
            to
        } else if from < a && a <= to {
            a - 1
        } else if to <= a && a < from {
            a + 1
        } else {
            a
        }
    }

    fn close_other_tabs(&mut self, keep: usize) {
        let Some(keep_id) = self.tabs.get(keep).map(|t| t.id) else { return };
        let ids: Vec<TabId> = self.tabs.iter().filter(|t| t.id != keep_id && !t.pinned).map(|t| t.id).collect();
        for id in ids {
            if let Some(i) = self.index_of(id) {
                self.close_tab(i);
            }
        }
    }

    fn close_tabs_to_right(&mut self, from: usize) {
        let ids: Vec<TabId> =
            self.tabs.iter().skip(from + 1).filter(|t| !t.pinned).map(|t| t.id).collect();
        for id in ids {
            if let Some(i) = self.index_of(id) {
                self.close_tab(i);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Import

    fn refresh_import(&mut self) {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
        let roaming = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
        self.import_profiles = import::detect_profiles(&local, &roaming);
        let rows: Vec<ImportProfileData> = self
            .import_profiles
            .iter()
            .enumerate()
            .map(|(i, p)| ImportProfileData { id: i as i32, label: p.label().into(), selected: true })
            .collect();
        self.models.import_profiles.set_vec(rows);
        if self.import_profiles.is_empty() {
            self.ui.set_import_message("".into());
        }
    }

    fn run_import(&mut self, want_bookmarks: bool, want_history: bool) {
        let selected: Vec<usize> = self
            .models
            .import_profiles
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.id as usize)
            .filter(|&i| i < self.import_profiles.len())
            .collect();
        if selected.is_empty() {
            self.ui.set_import_message("請先勾選至少一個設定檔。".into());
            return;
        }
        let what = import::Selection { bookmarks: want_bookmarks, history: want_history };
        let scratch = self.paths.root.join("import-tmp");
        let (mut n_bm, mut n_hist, mut errors, mut icons) = (0usize, 0usize, Vec::new(), Vec::new());
        let now = storage::now_secs();
        for i in selected {
            let profile = self.import_profiles[i].clone();
            let data = import::read_profile(&profile, what, &scratch);
            errors.extend(data.errors);
            icons.extend(data.icons);
            for b in data.bookmarks {
                if self.bookmarks.contains(&b.url) {
                    self.bookmarks.file_if_loose(&b.url, &b.folder);
                } else {
                    self.bookmarks.add(&b.url, &b.title, &b.folder);
                    n_bm += 1;
                }
            }
            for v in data.history {
                self.history.record(&v.url, &v.title, if v.last_visit > 0 { v.last_visit } else { now });
                n_hist += 1;
            }
        }
        let _ = std::fs::remove_dir_all(&scratch);
        // Icons for what is on screen right after the import (bookmarks, new-tab tiles), so they
        // don't stay globes until each site is visited. The rest arrive as sites are visited.
        let hosts: HashSet<String> = self
            .bookmarks
            .items()
            .iter()
            .map(|b| b.url.as_str())
            .chain(self.history.top_sites(8).into_iter().map(|h| h.url.as_str()))
            .filter_map(url_input::host_of)
            .collect();
        self.favicons.import(&icons, &hosts);
        self.bookmarks.save_if_dirty();
        self.history.save_if_dirty();
        self.refresh_bookmarks();
        self.refresh_top_sites();
        let mut msg = format!("已匯入 {n_bm} 個書籤、{n_hist} 筆歷史紀錄。");
        if !errors.is_empty() {
            msg.push('\n');
            msg.push_str(&errors.join("\n"));
        }
        self.ui.set_import_message(msg.into());
    }

    fn import_passwords_csv(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        let Some(path) = platform::open_csv_dialog(hwnd) else { return };
        // Validate the CSV now so the user gets immediate feedback.
        match import::csv::read_logins_file(&path) {
            Ok(logins) => {
                let pending_path = self.paths.root.join("pending-import.json");
                let mut pending = import::PendingImport::load(&pending_path);
                pending.add_csv(path);
                let _ = pending.save(&pending_path);
                self.ui.set_import_message(
                    format!("已排入 {} 筆密碼，請重新啟動 LiteBrowser 以完成匯入。", logins.len()).into(),
                );
            }
            Err(e) => self.ui.set_import_message(format!("讀取 CSV 失敗：{e}").into()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // MCP tool calls (run on the UI thread; see `mcp_dispatch`)

    /// Resolves an optional tab id to an index, defaulting to the active tab.
    fn mcp_tab_index(&self, tab: Option<TabId>) -> Result<usize, String> {
        match tab {
            Some(id) => self.index_of(id).ok_or_else(|| format!("找不到分頁 {id}")),
            None => (!self.tabs.is_empty()).then_some(self.active).ok_or_else(|| "沒有開啟的分頁".to_string()),
        }
    }

    /// Returns the tab's WebView, creating it first if the tab was discarded.
    fn mcp_live_view(&mut self, idx: usize) -> Result<&WebView, String> {
        if self.tabs[idx].view.is_none() {
            if self.tabs[idx].url.is_empty() {
                return Err("這個分頁還沒有載入網頁".into());
            }
            self.ensure_view(idx);
            return Err("分頁正在重新載入，請稍候再試一次".into());
        }
        self.tabs[idx].view.as_ref().ok_or_else(|| "分頁沒有網頁".to_string())
    }

    fn run_mcp_call(&mut self, call: mcp::Call, answer: McpAnswer) {
        use mcp::Call;
        let reply = |answer: &McpAnswer, value: Result<serde_json::Value, String>| {
            let _ = answer.send(value);
        };
        match call {
            Call::ListTabs => {
                let tabs: Vec<serde_json::Value> = self
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        serde_json::json!({
                            "tab_id": t.id,
                            "title": t.title,
                            "url": t.url,
                            "active": i == self.active,
                            "pinned": t.pinned,
                            "loaded": t.view.is_some(),
                        })
                    })
                    .collect();
                reply(&answer, Ok(serde_json::json!({ "tabs": tabs })));
            }
            Call::NewTab { url, activate } => {
                let url = url
                    .and_then(|u| url_input::to_url(&u, &self.cfg.search_url))
                    .unwrap_or_default();
                let at = self.tabs.len();
                let idx = self.insert_tab(at, url, String::new(), None);
                let id = self.tabs[idx].id;
                if activate {
                    self.activate(idx);
                } else {
                    self.ensure_view(idx);
                    self.refresh_all();
                }
                reply(&answer, Ok(serde_json::json!({ "tab_id": id })));
            }
            Call::CloseTab { tab } => match self.index_of(tab) {
                Some(idx) => {
                    self.close_tab(idx);
                    reply(&answer, Ok(serde_json::json!({ "closed": tab })));
                }
                None => reply(&answer, Err(format!("找不到分頁 {tab}"))),
            },
            Call::ActivateTab { tab } => match self.index_of(tab) {
                Some(idx) => {
                    self.activate(idx);
                    reply(&answer, Ok(serde_json::json!({ "active": tab })));
                }
                None => reply(&answer, Err(format!("找不到分頁 {tab}"))),
            },
            Call::Navigate { tab, url } => {
                let Some(url) = url_input::to_url(&url, &self.cfg.search_url) else {
                    reply(&answer, Err("網址是空的".into()));
                    return;
                };
                match self.mcp_tab_index(tab) {
                    Ok(idx) => {
                        if idx == self.active {
                            self.navigate_active(url.clone());
                        } else {
                            self.tabs[idx].url = url.clone();
                            self.tabs[idx].loading = true;
                            match self.tabs[idx].view.as_ref() {
                                Some(view) => view.navigate(&url),
                                None => self.ensure_view(idx),
                            }
                            self.refresh_all();
                        }
                        reply(&answer, Ok(serde_json::json!({ "url": url })));
                    }
                    Err(e) => reply(&answer, Err(e)),
                }
            }
            Call::Back { tab } | Call::Forward { tab } | Call::Reload { tab } => {
                let is_back = matches!(call, Call::Back { .. });
                let is_forward = matches!(call, Call::Forward { .. });
                match self.mcp_tab_index(tab).and_then(|idx| self.mcp_live_view(idx)) {
                    Ok(view) => {
                        if is_back {
                            view.go_back();
                        } else if is_forward {
                            view.go_forward();
                        } else {
                            view.reload();
                        }
                        reply(&answer, Ok(serde_json::json!({ "ok": true })));
                    }
                    Err(e) => reply(&answer, Err(e)),
                }
            }
            Call::PageText { tab } => self.mcp_script(tab, "document.body ? document.body.innerText : ''", answer),
            Call::PageHtml { tab } => self.mcp_script(tab, "document.documentElement.outerHTML", answer),
            Call::ExecuteJs { tab, script } => self.mcp_script(tab, &script, answer),
            Call::Screenshot { tab } => {
                // CapturePreview never completes for a hidden WebView, which would leave the
                // caller hanging until the MCP timeout; say so up front instead.
                let shown = |idx: usize| {
                    if idx == self.active && self.web_shown() {
                        Ok(idx)
                    } else {
                        Err("只能截取畫面上正在顯示的分頁；請先用 activate_tab 切換過去".to_string())
                    }
                };
                match self.mcp_tab_index(tab).and_then(shown).and_then(|idx| self.mcp_live_view(idx)) {
                    Ok(view) => view.capture_png(move |result| {
                        let _ = answer.send(result.map(|png| {
                            use base64::Engine;
                            serde_json::json!({
                                "png_base64": base64::engine::general_purpose::STANDARD.encode(png)
                            })
                        }));
                    }),
                    Err(e) => reply(&answer, Err(e)),
                }
            }
        }
    }

    /// Runs a script in a tab and answers with its value. WebView2 hands back JSON, so a string
    /// result is unwrapped to plain text and anything else is passed through as-is.
    fn mcp_script(&mut self, tab: Option<TabId>, script: &str, answer: McpAnswer) {
        match self.mcp_tab_index(tab).and_then(|idx| self.mcp_live_view(idx)) {
            Ok(view) => view.execute_script(script, move |result| {
                let value = result.map(|json| {
                    serde_json::from_str::<serde_json::Value>(&json).unwrap_or(serde_json::Value::String(json))
                });
                let _ = answer.send(value);
            }),
            Err(e) => {
                let _ = answer.send(Err(e));
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Docked DevTools

    fn toggle_devtools(&mut self) {
        if self.devtools_view.is_some() || self.devtools_creating {
            self.close_devtools();
            return;
        }
        if self.page != Page::Web || self.tabs.get(self.active).map(|t| t.view.is_none()).unwrap_or(true) {
            self.set_status("請先開啟網頁再使用開發人員工具");
            return;
        }
        let (Some(env), Some(hwnd)) = (self.env.clone(), self.hwnd) else { return };
        let page_url = self.tabs[self.active].url.clone();
        let frontend = match self.devtools_frontend_url(&page_url) {
            Ok(url) => url,
            Err(e) => {
                log!("docked devtools unavailable ({e}); opening a window instead");
                self.with_active_view(WebView::open_devtools_window);
                return;
            }
        };
        let started =
            webview::create_controller(&env, hwnd, false, |result| post(Event::DevtoolsViewCreated(result)));
        if let Err(e) = started {
            log!("devtools controller failed: {e}");
            self.with_active_view(WebView::open_devtools_window);
            return;
        }
        self.devtools_creating = true;
        self.pending_devtools_url = Some(frontend);
        self.ui.set_devtools_open(true);
    }

    fn devtools_frontend_url(&self, page_url: &str) -> Result<String, String> {
        let port = devtools::read_active_port(&self.paths.webview_data().join("EBWebView"))
            .or_else(|| devtools::read_active_port(&self.paths.webview_data()))
            .ok_or("找不到 DevTools 連接埠")?;
        let targets = devtools::fetch_targets(port)?;
        let target = devtools::pick_target(&targets, page_url).ok_or("找不到可偵錯的網頁")?;
        devtools::frontend_url(port, target).ok_or_else(|| "無法組出 DevTools 網址".to_string())
    }

    fn on_devtools_view_created(&mut self, result: Result<ICoreWebView2Controller, String>) {
        self.devtools_creating = false;
        // The user may have toggled DevTools off while the controller was being created.
        if !self.ui.get_devtools_open() {
            if let Ok(controller) = result {
                unsafe {
                    let _ = controller.Close();
                }
            }
            return;
        }
        let Some(env) = self.env.clone() else { return };
        // DevTools gets a dummy tab id (0) — it never participates in the memory policy.
        match result.and_then(|c| WebView::attach(c, &env, 0, None).map_err(|e| e.message().to_string())) {
            Ok(view) => {
                if let Some(url) = self.pending_devtools_url.take() {
                    view.navigate(&url);
                }
                self.devtools_view = Some(view);
                self.layout_views();
            }
            Err(e) => {
                log!("devtools attach failed: {e}");
                self.ui.set_devtools_open(false);
                self.with_active_view(WebView::open_devtools_window);
            }
        }
    }

    /// Chrome's ⋮ menu, hung from the button's bottom-right corner.
    fn main_menu(&mut self, x: f32, y: f32) {
        let bar = self.cfg.show_bookmarks_bar;
        let menu = vec![
            MenuItem::entry(1, "新分頁\tCtrl+T"),
            MenuItem::entry(2, "新無痕分頁\tCtrl+Shift+N"),
            MenuItem::Separator,
            MenuItem::entry(3, "歷史紀錄\tCtrl+H"),
            MenuItem::entry(4, "下載\tCtrl+J"),
            MenuItem::entry(5, "書籤\tCtrl+Shift+O"),
            MenuItem::Entry { id: 6, label: "顯示書籤列".into(), checked: bar, enabled: true, icon: None },
            if self.closed.is_empty() {
                MenuItem::disabled("重新開啟關閉的分頁\tCtrl+Shift+T")
            } else {
                MenuItem::entry(7, "重新開啟關閉的分頁\tCtrl+Shift+T")
            },
            MenuItem::Separator,
            MenuItem::entry(8, "全螢幕\tF11"),
            MenuItem::entry(9, "開發人員工具\tF12"),
            MenuItem::Separator,
            MenuItem::entry(10, "擴充功能"),
            MenuItem::entry(11, "從其他瀏覽器匯入…"),
            MenuItem::entry(12, "設定"),
            MenuItem::Separator,
            MenuItem::entry(13, "結束"),
        ];
        let scale = self.ui.window().scale_factor();
        match platform::popup_menu_below(&menu, (x * scale).round() as i32, (y * scale).round() as i32, true) {
            1 => self.run_shortcut(Shortcut::NewTab),
            2 => self.run_shortcut(Shortcut::NewPrivateTab),
            3 => self.run_shortcut(Shortcut::ShowHistory),
            4 => self.run_shortcut(Shortcut::ShowDownloads),
            5 => self.run_shortcut(Shortcut::ShowBookmarks),
            6 => {
                let mut s = self.settings_data();
                s.show_bookmarks_bar = !bar;
                self.save_settings(s);
            }
            7 => self.run_shortcut(Shortcut::ReopenClosedTab),
            8 => self.run_shortcut(Shortcut::ToggleFullScreen),
            9 => self.run_shortcut(Shortcut::ToggleDevtools),
            10 => self.show_page(Page::Extensions),
            11 => self.show_page(Page::Import),
            12 => self.show_page(Page::Settings),
            13 => self.quit(),
            _ => {}
        }
    }

    /// The omnibox's edit menu, native so it is not hidden under the WebView like a Slint popup.
    fn address_menu(&mut self) {
        let menu = vec![
            MenuItem::entry(1, "復原"),
            MenuItem::Separator,
            MenuItem::entry(2, "剪下"),
            MenuItem::entry(3, "複製"),
            MenuItem::entry(4, "貼上"),
            MenuItem::Separator,
            MenuItem::entry(5, "全選"),
        ];
        let action = match platform::popup_menu(&menu) {
            1 => "undo",
            2 => "cut",
            3 => "copy",
            4 => "paste",
            5 => "select-all",
            _ => return,
        };
        self.ui.invoke_address_action(action.into());
    }

    /// Same as the old system close button: the session is kept for next time.
    fn quit(&mut self) {
        self.save_all();
        slint::quit_event_loop().ok();
    }

    /// Right-clicking the DevTools splitter offers the dock side (and closing it).
    fn devtools_dock_menu(&mut self) {
        let right = self.cfg.devtools_dock_right;
        let menu = vec![
            MenuItem::entry(1, if right { "停駐在下方" } else { "停駐在下方（目前）" }),
            MenuItem::entry(2, if right { "停駐在右側（目前）" } else { "停駐在右側" }),
            MenuItem::Separator,
            MenuItem::entry(3, "關閉開發人員工具"),
        ];
        match platform::popup_menu(&menu) {
            1 => self.set_devtools_side(false),
            2 => self.set_devtools_side(true),
            3 => self.close_devtools(),
            _ => {}
        }
    }

    fn set_devtools_side(&mut self, right: bool) {
        if self.cfg.devtools_dock_right == right {
            return;
        }
        self.cfg.devtools_dock_right = right;
        let _ = self.cfg.save(&self.paths.config());
        self.ui.set_devtools_right(right);
        self.ui.set_settings(self.settings_data());
        self.layout_views();
    }

    fn close_devtools(&mut self) {
        self.devtools_view = None; // dropping the WebView closes its controller
        self.devtools_creating = false;
        self.pending_devtools_url = None;
        self.ui.set_devtools_open(false);
        self.layout_views();
    }
}
