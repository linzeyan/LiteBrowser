//! Thin wrapper over the WebView2 COM API.
//!
//! Every WebView2 event handler only posts an [`EngineEvent`] to the app's queue; all state
//! changes happen later in `App::handle`, so handlers never re-enter app state.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, AcceleratorKeyPressedEventHandler, BrowserExtensionEnableCompletedHandler, CapturePreviewCompletedHandler,
    ContainsFullScreenElementChangedEventHandler,
    ExecuteScriptCompletedHandler, GetProcessExtendedInfosCompletedHandler,
    BrowserExtensionRemoveCompletedHandler, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DocumentTitleChangedEventHandler, FaviconChangedEventHandler,
    GetFaviconCompletedHandler, HistoryChangedEventHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler, ProcessFailedEventHandler,
    ProfileAddBrowserExtensionCompletedHandler, ProfileGetBrowserExtensionsCompletedHandler, SourceChangedEventHandler,
    TrySuspendCompletedHandler, WebResourceRequestedEventHandler, WindowCloseRequestedEventHandler,
};
use windows::core::{Interface, BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::IStream;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MBUTTON, VK_MENU, VK_SHIFT};

use crate::adblock::Blocklist;
use crate::app::{post, Event};
use crate::logging::log;
use crate::shortcuts::{self, Modifiers, Shortcut};
use crate::tabs::TabId;

thread_local! {
    static BLOCKED_REQUESTS: Cell<u64> = const { Cell::new(0) };
    /// True while the *browser* is full screen (F11), which is the only time Esc is taken from
    /// the page. A page's own full screen is left alone: it exits on Esc by itself.
    static BROWSER_FULLSCREEN: Cell<bool> = const { Cell::new(false) };
}

pub fn set_browser_fullscreen(on: bool) {
    BROWSER_FULLSCREEN.with(|flag| flag.set(on));
}

pub fn blocked_requests() -> u64 {
    BLOCKED_REQUESTS.with(Cell::get)
}

pub enum EngineEvent {
    TitleChanged(String),
    SourceChanged(String),
    NavigationStarting,
    NavigationCompleted,
    HistoryChanged { can_back: bool, can_forward: bool },
    NewWindow(NewWindowRequest),
    CloseRequested,
    RendererGone,
    BrowserGone,
    Shortcut(Shortcut),
    SuspendFinished(bool),
    /// PNG bytes of the page's favicon.
    Favicon(Vec<u8>),
    /// The page entered or left HTML5 full screen (a video player, a slide deck).
    FullScreen(bool),
}

/// `window.open()` / target=_blank / "open in new window" from a page.
pub struct NewWindowRequest {
    pub uri: String,
    pub args: ICoreWebView2NewWindowRequestedEventArgs,
    pub deferral: ICoreWebView2Deferral,
    /// Ctrl/middle click: open in a background tab without loading it yet.
    pub background: bool,
}

impl NewWindowRequest {
    /// Tells WebView2 we handled the request without creating a window.
    pub fn complete_handled(&self) {
        unsafe {
            let _ = self.args.SetHandled(true);
            let _ = self.deferral.Complete();
        }
    }
}

const MIN_RUNTIME_VERSION: &str = "100.0.1185.36";

fn post_engine(tab: TabId, event: EngineEvent) {
    post(Event::Engine(tab, event));
}

/// Reads an `IStream` (e.g. a favicon PNG) fully into memory.
fn read_stream(stream: &IStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let mut read = 0u32;
        let hr = unsafe { stream.Read(chunk.as_mut_ptr() as *mut _, chunk.len() as u32, Some(&mut read)) };
        if read > 0 {
            out.extend_from_slice(&chunk[..read as usize]);
        }
        // Stop at end of stream (0 bytes) or on a hard error; a favicon is never this large.
        if read == 0 || hr.is_err() || out.len() > 2 * 1024 * 1024 {
            break;
        }
    }
    out
}

fn key_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

fn modifiers() -> Modifiers {
    Modifiers { ctrl: key_down(VK_CONTROL.0), shift: key_down(VK_SHIFT.0), alt: key_down(VK_MENU.0) }
}

fn hresult_message(e: &windows::core::Error) -> String {
    format!("{} (0x{:08X})", e.message(), e.code().0)
}

/// Starts creating the shared WebView2 environment (browser process). `done` runs on the UI
/// thread once it is ready. An immediate error usually means the WebView2 Runtime is missing.
pub fn create_environment(
    user_data: &Path,
    browser_args: String,
    done: impl FnOnce(Result<ICoreWebView2Environment, String>) + 'static,
) -> Result<(), String> {
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe {
        // The crate defaults to the SDK version (Edge 145+). Company VDIs often lag behind, so
        // accept any runtime from 100 on; newer features are detected with `cast()` at use.
        options.set_target_compatible_browser_version(MIN_RUNTIME_VERSION.to_string());
        options.set_additional_browser_arguments(browser_args);
        options.set_are_browser_extensions_enabled(true);
    }
    let options = ICoreWebView2EnvironmentOptions::from(options);
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, env| {
        done(match (result, env) {
            (Ok(()), Some(env)) => Ok(env),
            (Err(e), _) => Err(hresult_message(&e)),
            (Ok(()), None) => Err("WebView2 沒有回傳環境".into()),
        });
        Ok(())
    }));
    let folder = HSTRING::from(user_data.as_os_str());
    unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &folder, &options, &handler) }
        .map_err(|e| hresult_message(&e))
}

/// Starts creating a WebView2 controller as a child of `parent`.
pub fn create_controller(
    env: &ICoreWebView2Environment,
    parent: HWND,
    private: bool,
    done: impl FnOnce(Result<ICoreWebView2Controller, String>) + 'static,
) -> Result<(), String> {
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
        done(match (result, controller) {
            (Ok(()), Some(c)) => Ok(c),
            (Err(e), _) => Err(hresult_message(&e)),
            (Ok(()), None) => Err("WebView2 沒有回傳控制器".into()),
        });
        Ok(())
    }));
    unsafe {
        // A private tab gets a controller whose profile is in-memory: no cookies, cache or history
        // survive it. Needs ICoreWebView2Environment10; older runtimes simply get a normal tab.
        if private {
            if let Ok(env10) = env.cast::<ICoreWebView2Environment10>() {
                let options = env10.CreateCoreWebView2ControllerOptions().map_err(|e| hresult_message(&e))?;
                options.SetIsInPrivateModeEnabled(true).map_err(|e| hresult_message(&e))?;
                return env10
                    .CreateCoreWebView2ControllerWithOptions(parent, &options, &handler)
                    .map_err(|e| hresult_message(&e));
            }
            return Err("這個版本的 WebView2 Runtime 不支援無痕模式".into());
        }
        env.CreateCoreWebView2Controller(parent, &handler).map_err(|e| hresult_message(&e))
    }
}

pub struct WebView {
    pub controller: ICoreWebView2Controller,
    pub core: ICoreWebView2,
}

impl Drop for WebView {
    fn drop(&mut self) {
        unsafe {
            let _ = self.controller.Close();
        }
    }
}

impl WebView {
    /// Configures a freshly created controller and hooks up all events for tab `tab`.
    pub fn attach(
        controller: ICoreWebView2Controller,
        env: &ICoreWebView2Environment,
        tab: TabId,
        blocklist: Option<Rc<Blocklist>>,
    ) -> windows::core::Result<Self> {
        unsafe {
            controller.SetIsVisible(false)?;
            let core = controller.CoreWebView2()?;
            // From here on, an early `?` return drops `view`, which closes the controller.
            let view = Self { controller, core };
            let (controller, core) = (&view.controller, &view.core);

            let settings = core.Settings()?;
            settings.SetIsStatusBarEnabled(true)?;
            settings.SetAreDevToolsEnabled(true)?;
            settings.SetAreDefaultContextMenusEnabled(true)?;
            settings.SetIsZoomControlEnabled(true)?;
            if let Ok(s4) = settings.cast::<ICoreWebView2Settings4>() {
                let _ = s4.SetIsPasswordAutosaveEnabled(true);
                let _ = s4.SetIsGeneralAutofillEnabled(true);
            }

            let mut token = 0i64;

            core.add_DocumentTitleChanged(
                &DocumentTitleChangedEventHandler::create(Box::new(move |sender, _| {
                    if let Some(wv) = sender {
                        let mut title = PWSTR::null();
                        wv.DocumentTitle(&mut title)?;
                        post_engine(tab, EngineEvent::TitleChanged(take_pwstr(title)));
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_SourceChanged(
                &SourceChangedEventHandler::create(Box::new(move |sender, _| {
                    if let Some(wv) = sender {
                        let mut uri = PWSTR::null();
                        wv.Source(&mut uri)?;
                        post_engine(tab, EngineEvent::SourceChanged(take_pwstr(uri)));
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_NavigationStarting(
                &NavigationStartingEventHandler::create(Box::new(move |_, _| {
                    post_engine(tab, EngineEvent::NavigationStarting);
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_NavigationCompleted(
                &NavigationCompletedEventHandler::create(Box::new(move |_, _| {
                    post_engine(tab, EngineEvent::NavigationCompleted);
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_HistoryChanged(
                &HistoryChangedEventHandler::create(Box::new(move |sender, _| {
                    if let Some(wv) = sender {
                        let (mut back, mut fwd) = (BOOL(0), BOOL(0));
                        wv.CanGoBack(&mut back)?;
                        wv.CanGoForward(&mut fwd)?;
                        post_engine(
                            tab,
                            EngineEvent::HistoryChanged { can_back: back.as_bool(), can_forward: fwd.as_bool() },
                        );
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_NewWindowRequested(
                &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else { return Ok(()) };
                    let mut uri = PWSTR::null();
                    let (mut has_size, mut has_pos) = (BOOL(0), BOOL(0));
                    args.Uri(&mut uri)?;
                    if let Ok(features) = args.WindowFeatures() {
                        let _ = features.HasSize(&mut has_size);
                        let _ = features.HasPosition(&mut has_pos);
                    }
                    let deferral = args.GetDeferral()?;
                    // A sized/positioned window is a popup (e.g. a login dialog) that needs window.opener,
                    // so it always gets a live WebView. Ctrl/middle clicks become lazy background tabs.
                    let popup = has_size.as_bool() || has_pos.as_bool();
                    let background = !popup && (modifiers().ctrl || key_down(VK_MBUTTON.0));
                    post_engine(
                        tab,
                        EngineEvent::NewWindow(NewWindowRequest {
                            uri: take_pwstr(uri),
                            args,
                            deferral,
                            background,
                        }),
                    );
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_ContainsFullScreenElementChanged(
                &ContainsFullScreenElementChangedEventHandler::create(Box::new(move |sender, _| {
                    if let Some(wv) = sender {
                        let mut full = BOOL::from(false);
                        wv.ContainsFullScreenElement(&mut full)?;
                        post_engine(tab, EngineEvent::FullScreen(full.as_bool()));
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_WindowCloseRequested(
                &WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
                    post_engine(tab, EngineEvent::CloseRequested);
                    Ok(())
                })),
                &mut token,
            )?;

            core.add_ProcessFailed(
                &ProcessFailedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else { return Ok(()) };
                    let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                    args.ProcessFailedKind(&mut kind)?;
                    log!("tab {tab}: WebView2 process failed, kind {}", kind.0);
                    if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED {
                        post_engine(tab, EngineEvent::BrowserGone);
                    } else if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED {
                        post_engine(tab, EngineEvent::RendererGone);
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            controller.add_AcceleratorKeyPressed(
                &AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else { return Ok(()) };
                    let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
                    let mut vk = 0u32;
                    let mut status = COREWEBVIEW2_PHYSICAL_KEY_STATUS::default();
                    args.KeyEventKind(&mut kind)?;
                    if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN {
                        return Ok(());
                    }
                    args.VirtualKey(&mut vk)?;
                    args.PhysicalKeyStatus(&mut status)?;
                    let shortcut = if BROWSER_FULLSCREEN.with(Cell::get) {
                        shortcuts::from_virtual_key_in_fullscreen(vk, modifiers())
                    } else {
                        shortcuts::from_virtual_key(vk, modifiers())
                    };
                    if let Some(shortcut) = shortcut {
                        args.SetHandled(true)?;
                        let repeat = status.WasKeyDown.as_bool();
                        if !repeat || matches!(shortcut, Shortcut::NextTab | Shortcut::PrevTab) {
                            post_engine(tab, EngineEvent::Shortcut(shortcut));
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;

            // Favicons: ICoreWebView2_15 (newer runtimes). Ignored on older ones.
            if let Ok(wv15) = core.cast::<ICoreWebView2_15>() {
                let wv15_for_fetch = wv15.clone();
                wv15.add_FaviconChanged(
                    &FaviconChangedEventHandler::create(Box::new(move |_, _| {
                        let handler = GetFaviconCompletedHandler::create(Box::new(move |result, stream| {
                            if result.is_ok() {
                                if let Some(stream) = stream {
                                    let bytes = read_stream(&stream);
                                    if !bytes.is_empty() {
                                        post_engine(tab, EngineEvent::Favicon(bytes));
                                    }
                                }
                            }
                            Ok(())
                        }));
                        let _ = wv15_for_fetch.GetFavicon(COREWEBVIEW2_FAVICON_IMAGE_FORMAT_PNG, &handler);
                        Ok(())
                    })),
                    &mut token,
                )?;
            }

            if let Some(blocklist) = blocklist {
                install_blocker(core, env, blocklist)?;
            }

            Ok(view)
        }
    }

    pub fn navigate(&self, url: &str) {
        if let Err(e) = unsafe { self.core.Navigate(&HSTRING::from(url)) } {
            // No URL in the log: it may be a private tab's.
            log!("navigate failed: {}", hresult_message(&e));
        }
    }

    pub fn go_back(&self) {
        unsafe {
            let _ = self.core.GoBack();
        }
    }

    pub fn go_forward(&self) {
        unsafe {
            let _ = self.core.GoForward();
        }
    }

    pub fn reload(&self) {
        unsafe {
            let _ = self.core.Reload();
        }
    }

    pub fn stop(&self) {
        unsafe {
            let _ = self.core.Stop();
        }
    }

    pub fn set_bounds(&self, bounds: RECT) {
        unsafe {
            let _ = self.controller.SetBounds(bounds);
        }
    }

    pub fn set_visible(&self, visible: bool) {
        unsafe {
            let _ = self.controller.SetIsVisible(visible);
        }
    }

    pub fn focus(&self) {
        unsafe {
            let _ = self.controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
        }
    }

    /// Freezes the page (must already be hidden). Falls back to the low memory target if the
    /// page refuses to suspend (e.g. it plays audio).
    pub fn try_suspend(&self, tab: TabId) {
        let Ok(wv3) = self.core.cast::<ICoreWebView2_3>() else { return };
        let handler = TrySuspendCompletedHandler::create(Box::new(move |result, ok| {
            post_engine(tab, EngineEvent::SuspendFinished(result.is_ok() && ok));
            Ok(())
        }));
        unsafe {
            if wv3.TrySuspend(&handler).is_err() {
                post_engine(tab, EngineEvent::SuspendFinished(false));
            }
        }
    }

    pub fn resume(&self) {
        if let Ok(wv3) = self.core.cast::<ICoreWebView2_3>() {
            unsafe {
                let _ = wv3.Resume();
            }
        }
    }

    pub fn set_low_memory(&self, low: bool) {
        if let Ok(wv19) = self.core.cast::<ICoreWebView2_19>() {
            let level =
                if low { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL };
            unsafe {
                let _ = wv19.SetMemoryUsageTargetLevel(level);
            }
        }
    }

    /// Runs JavaScript in the page. The result is the script's value encoded as JSON
    /// ("null" when it has none), matching WebView2's own ExecuteScript contract.
    pub fn execute_script(&self, script: &str, done: impl FnOnce(Result<String, String>) + 'static) {
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |result, json| {
            done(match result {
                Ok(()) => Ok(json),
                Err(e) => Err(hresult_message(&e)),
            });
            Ok(())
        }));
        if let Err(e) = unsafe { self.core.ExecuteScript(&HSTRING::from(script), &handler) } {
            log!("ExecuteScript failed: {}", hresult_message(&e));
        }
    }

    /// Captures the page as a PNG.
    pub fn capture_png(&self, done: impl FnOnce(Result<Vec<u8>, String>) + 'static) {
        let Some(stream) = (unsafe { windows::Win32::UI::Shell::SHCreateMemStream(None) }) else {
            done(Err("無法建立影像緩衝區".into()));
            return;
        };
        let for_read = stream.clone();
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            done(match result {
                Ok(()) => {
                    // Rewind before reading: CapturePreview leaves the stream at the end.
                    unsafe {
                        let _ = for_read.Seek(0, windows::Win32::System::Com::STREAM_SEEK_SET, None);
                    }
                    Ok(read_stream(&for_read))
                }
                Err(e) => Err(hresult_message(&e)),
            });
            Ok(())
        }));
        let started = unsafe {
            self.core.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler)
        };
        if let Err(e) = started {
            log!("CapturePreview failed: {}", hresult_message(&e));
        }
    }

    /// Opens WebView2's own DevTools window (fallback when docked DevTools can't be built).
    pub fn open_devtools_window(&self) {
        unsafe {
            let _ = self.core.OpenDevToolsWindow();
        }
    }

    pub fn open_downloads(&self) -> bool {
        match self.core.cast::<ICoreWebView2_9>() {
            Ok(wv9) => unsafe { wv9.OpenDefaultDownloadDialog().is_ok() },
            Err(_) => false,
        }
    }

    pub fn profile(&self) -> Option<ICoreWebView2Profile7> {
        let wv13 = self.core.cast::<ICoreWebView2_13>().ok()?;
        unsafe { wv13.Profile().ok()?.cast::<ICoreWebView2Profile7>().ok() }
    }

    /// The main frame's id, the same kind of id [`renderer_frames`] reports. Needs ICoreWebView2_20.
    pub fn frame_id(&self) -> Option<u32> {
        let wv20 = self.core.cast::<ICoreWebView2_20>().ok()?;
        let mut id = 0;
        unsafe { wv20.FrameId(&mut id) }.ok()?;
        Some(id)
    }
}

/// Which frames each renderer process runs: `done` gets every renderer's PID with the ids of its
/// frames. Needs ICoreWebView2Environment13; older runtimes never call `done`.
// ponytail: only a page's main frame matches a tab, so a cross-site iframe in a renderer of its
// own isn't counted for its page; walk `ICoreWebView2FrameInfo2::ParentFrameInfo` up to the main
// frame if the largest-first ranking turns out wrong.
pub fn renderer_frames(env: &ICoreWebView2Environment, done: impl FnOnce(Vec<(u32, Vec<u32>)>) + 'static) {
    let Ok(env13) = env.cast::<ICoreWebView2Environment13>() else { return };
    let handler = GetProcessExtendedInfosCompletedHandler::create(Box::new(move |result, infos| {
        if let (Ok(()), Some(infos)) = (result, infos) {
            done(unsafe { read_renderers(&infos) });
        }
        Ok(())
    }));
    unsafe {
        let _ = env13.GetProcessExtendedInfos(&handler);
    }
}

unsafe fn read_renderers(infos: &ICoreWebView2ProcessExtendedInfoCollection) -> Vec<(u32, Vec<u32>)> {
    let mut out = Vec::new();
    let mut count = 0;
    let _ = infos.Count(&mut count);
    for i in 0..count {
        let Ok(info) = infos.GetValueAtIndex(i) else { continue };
        let Ok(process) = info.ProcessInfo() else { continue };
        let (mut kind, mut pid) = (COREWEBVIEW2_PROCESS_KIND::default(), 0);
        if process.Kind(&mut kind).is_err() || kind != COREWEBVIEW2_PROCESS_KIND_RENDERER || process.ProcessId(&mut pid).is_err() {
            continue;
        }
        let mut frames = Vec::new();
        // Keep the collection alive while iterating: WebView2 154's iterator doesn't hold a
        // reference to it, and iterating after dropping it crashed in EmbeddedBrowserWebView.dll.
        let collection = info.AssociatedFrameInfos().ok();
        if let Some(iter) = collection.as_ref().and_then(|c| c.GetIterator().ok()) {
            let mut has = BOOL(0);
            while iter.HasCurrent(&mut has).is_ok() && has.as_bool() {
                if let Some(info2) = iter.GetCurrent().ok().and_then(|f| f.cast::<ICoreWebView2FrameInfo2>().ok()) {
                    let mut id = 0;
                    if info2.FrameId(&mut id).is_ok() && id != 0 {
                        frames.push(id);
                    }
                }
                if iter.MoveNext(&mut has).is_err() {
                    break;
                }
            }
        }
        out.push((pid as u32, frames));
    }
    out
}

/// Answers requests to blocked domains with an empty 403 instead of letting them out.
fn install_blocker(
    core: &ICoreWebView2,
    env: &ICoreWebView2Environment,
    blocklist: Rc<Blocklist>,
) -> windows::core::Result<()> {
    unsafe {
        let wv22 = core.cast::<ICoreWebView2_22>().ok();
        for pattern in blocklist.filter_patterns().iter().take(8000) {
            let pattern = HSTRING::from(pattern.as_str());
            match &wv22 {
                // Also covers service workers, which the legacy filter misses.
                Some(wv22) => wv22.AddWebResourceRequestedFilterWithRequestSourceKinds(
                    &pattern,
                    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
                    COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL,
                )?,
                None => core.AddWebResourceRequestedFilter(&pattern, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?,
            }
        }
        let env = env.clone();
        let mut token = 0i64;
        core.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut uri = PWSTR::null();
                args.Request()?.Uri(&mut uri)?;
                // The filter patterns are a coarse wildcard match; confirm the host really is listed.
                if blocklist.is_blocked_url(&take_pwstr(uri)) {
                    let response =
                        env.CreateWebResourceResponse(None::<&IStream>, 403, &HSTRING::from("Blocked"), &HSTRING::new())?;
                    args.SetResponse(&response)?;
                    BLOCKED_REQUESTS.with(|c| c.set(c.get() + 1));
                }
                Ok(())
            })),
            &mut token,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Extensions

#[derive(Clone, Debug)]
pub struct ExtensionInfo {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

fn extension_info(ext: &ICoreWebView2BrowserExtension) -> windows::core::Result<ExtensionInfo> {
    let (mut id, mut name, mut enabled) = (PWSTR::null(), PWSTR::null(), BOOL(0));
    unsafe {
        ext.Id(&mut id)?;
        ext.Name(&mut name)?;
        ext.IsEnabled(&mut enabled)?;
    }
    Ok(ExtensionInfo { id: take_pwstr(id), name: take_pwstr(name), enabled: enabled.as_bool() })
}

fn with_extension_list(
    profile: &ICoreWebView2Profile7,
    done: impl FnOnce(Result<Vec<ICoreWebView2BrowserExtension>, String>) + 'static,
) {
    let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |result, list| {
        let extensions = (|| -> windows::core::Result<Vec<ICoreWebView2BrowserExtension>> {
            result?;
            let mut out = Vec::new();
            if let Some(list) = list {
                let mut count = 0u32;
                unsafe {
                    list.Count(&mut count)?;
                    for i in 0..count {
                        out.push(list.GetValueAtIndex(i)?);
                    }
                }
            }
            Ok(out)
        })();
        done(extensions.map_err(|e| hresult_message(&e)));
        Ok(())
    }));
    if let Err(e) = unsafe { profile.GetBrowserExtensions(&handler) } {
        log!("GetBrowserExtensions failed: {}", hresult_message(&e));
    }
}

pub fn list_extensions(profile: &ICoreWebView2Profile7, done: impl FnOnce(Result<Vec<ExtensionInfo>, String>) + 'static) {
    with_extension_list(profile, move |result| {
        done(result.map(|list| list.iter().filter_map(|e| extension_info(e).ok()).collect()));
    });
}

/// Installs an unpacked extension folder; returns its id and name.
pub fn add_extension(
    profile: &ICoreWebView2Profile7,
    folder: &Path,
    done: impl FnOnce(Result<ExtensionInfo, String>) + 'static,
) {
    let handler = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(move |result, ext| {
        done(match (result, ext) {
            (Ok(()), Some(ext)) => extension_info(&ext).map_err(|e| hresult_message(&e)),
            (Err(e), _) => Err(hresult_message(&e)),
            (Ok(()), None) => Err("沒有回傳擴充功能".into()),
        });
        Ok(())
    }));
    let path = HSTRING::from(folder.as_os_str());
    if let Err(e) = unsafe { profile.AddBrowserExtension(&path, &handler) } {
        log!("AddBrowserExtension failed: {}", hresult_message(&e));
    }
}

pub enum ExtensionChange {
    Enable(bool),
    Remove,
}

pub fn change_extension(
    profile: &ICoreWebView2Profile7,
    id: String,
    change: ExtensionChange,
    done: impl FnOnce(Result<(), String>) + 'static,
) {
    with_extension_list(profile, move |result| {
        let ext = match result {
            Ok(list) => list.into_iter().find(|e| extension_info(e).is_ok_and(|i| i.id == id)),
            Err(e) => return done(Err(e)),
        };
        let Some(ext) = ext else { return done(Err("找不到這個擴充功能".into())) };
        let outcome = unsafe {
            match change {
                ExtensionChange::Enable(enabled) => {
                    let handler = BrowserExtensionEnableCompletedHandler::create(Box::new(move |result| {
                        done(result.map_err(|e| hresult_message(&e)));
                        Ok(())
                    }));
                    ext.Enable(enabled, &handler)
                }
                ExtensionChange::Remove => {
                    let handler = BrowserExtensionRemoveCompletedHandler::create(Box::new(move |result| {
                        done(result.map_err(|e| hresult_message(&e)));
                        Ok(())
                    }));
                    ext.Remove(&handler)
                }
            }
        };
        if let Err(e) = outcome {
            log!("extension change failed: {}", hresult_message(&e));
        }
    });
}
