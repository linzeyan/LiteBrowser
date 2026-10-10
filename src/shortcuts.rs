//! Browser keyboard shortcuts. The same commands arrive from two places:
//! Win32 virtual keys while the web page has focus, and named commands from the Slint UI.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    NewTab,
    NewPrivateTab,
    CloseTab,
    ReopenClosedTab,
    NextTab,
    PrevTab,
    /// Ctrl+1..8 (0-based index).
    SelectTab(usize),
    /// Ctrl+9
    LastTab,
    FocusAddress,
    ToggleBookmark,
    ShowBookmarks,
    ShowHistory,
    ShowDownloads,
    Reload,
    ToggleFullScreen,
    ExitFullScreen,
    ToggleDevtools,
    /// Ctrl+0: WebView2 zooms on Ctrl+± and the wheel by itself, but has no key back to 100 %.
    ResetZoom,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

const VK_TAB: u32 = 0x09;
const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;
const VK_F4: u32 = 0x73;
const VK_F6: u32 = 0x75;
const VK_F11: u32 = 0x7A;
const VK_F12: u32 = 0x7B;
const VK_ESCAPE: u32 = 0x1B;
const VK_NUMPAD0: u32 = 0x60;

/// Shortcuts we take away from the web page. Everything else (F5, Ctrl+F, Alt+Left, Ctrl+±…)
/// is left to WebView2's built-in handling.
pub fn from_virtual_key(vk: u32, m: Modifiers) -> Option<Shortcut> {
    let key = char::from_u32(vk).filter(|c| c.is_ascii_alphanumeric());
    match (m.ctrl, m.shift, m.alt) {
        (true, false, false) => match (vk, key) {
            (_, Some('T')) | (_, Some('N')) => Some(Shortcut::NewTab),
            (_, Some('W')) | (VK_F4, _) => Some(Shortcut::CloseTab),
            (_, Some('L')) => Some(Shortcut::FocusAddress),
            (_, Some('D')) => Some(Shortcut::ToggleBookmark),
            (_, Some('H')) => Some(Shortcut::ShowHistory),
            (_, Some('J')) => Some(Shortcut::ShowDownloads),
            (VK_TAB, _) | (VK_NEXT, _) => Some(Shortcut::NextTab),
            (VK_PRIOR, _) => Some(Shortcut::PrevTab),
            (_, Some(c @ '1'..='8')) => Some(Shortcut::SelectTab(c as usize - '1' as usize)),
            (_, Some('9')) => Some(Shortcut::LastTab),
            (_, Some('0')) | (VK_NUMPAD0, _) => Some(Shortcut::ResetZoom),
            _ => None,
        },
        (true, true, false) => match (vk, key) {
            (_, Some('N')) => Some(Shortcut::NewPrivateTab),
            (_, Some('T')) => Some(Shortcut::ReopenClosedTab),
            (_, Some('O')) => Some(Shortcut::ShowBookmarks),
            (_, Some('I')) => Some(Shortcut::ToggleDevtools),
            (VK_TAB, _) => Some(Shortcut::PrevTab),
            _ => None,
        },
        (false, false, true) if key == Some('D') => Some(Shortcut::FocusAddress),
        (false, false, false) if vk == VK_F6 => Some(Shortcut::FocusAddress),
        (false, false, false) if vk == VK_F11 => Some(Shortcut::ToggleFullScreen),
        // Left to the page, WebView2 would open its own floating DevTools window instead.
        (false, false, false) if vk == VK_F12 => Some(Shortcut::ToggleDevtools),
        _ => None,
    }
}

/// While the browser itself is full screen (F11), Esc leaves it. Any other time Esc belongs to
/// the page, which uses it to close its own dialogs and menus.
pub fn from_virtual_key_in_fullscreen(vk: u32, m: Modifiers) -> Option<Shortcut> {
    if vk == VK_ESCAPE && !m.ctrl && !m.shift && !m.alt {
        return Some(Shortcut::ExitFullScreen);
    }
    from_virtual_key(vk, m)
}

/// Names used by the Slint UI (`root.command("new-tab")`).
pub fn from_name(name: &str) -> Option<Shortcut> {
    Some(match name {
        "new-tab" => Shortcut::NewTab,
        "new-private-tab" => Shortcut::NewPrivateTab,
        "close-tab" => Shortcut::CloseTab,
        "reopen-tab" => Shortcut::ReopenClosedTab,
        "next-tab" => Shortcut::NextTab,
        "prev-tab" => Shortcut::PrevTab,
        "last-tab" => Shortcut::LastTab,
        "focus-address" => Shortcut::FocusAddress,
        "bookmark" => Shortcut::ToggleBookmark,
        "bookmarks" => Shortcut::ShowBookmarks,
        "history" => Shortcut::ShowHistory,
        "downloads" => Shortcut::ShowDownloads,
        "reload" => Shortcut::Reload,
        "fullscreen" => Shortcut::ToggleFullScreen,
        "exit-fullscreen" => Shortcut::ExitFullScreen,
        _ => {
            let n: usize = name.strip_prefix("tab-")?.parse().ok()?;
            Shortcut::SelectTab(n.checked_sub(1)?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: Modifiers = Modifiers { ctrl: true, shift: false, alt: false };
    const CTRL_SHIFT: Modifiers = Modifiers { ctrl: true, shift: true, alt: false };

    #[test]
    fn virtual_keys() {
        assert_eq!(from_virtual_key('T' as u32, CTRL), Some(Shortcut::NewTab));
        assert_eq!(from_virtual_key('T' as u32, CTRL_SHIFT), Some(Shortcut::ReopenClosedTab));
        assert_eq!(from_virtual_key('N' as u32, CTRL_SHIFT), Some(Shortcut::NewPrivateTab));
        assert_eq!(from_name("new-private-tab"), Some(Shortcut::NewPrivateTab));
        assert_eq!(from_virtual_key(VK_TAB, CTRL), Some(Shortcut::NextTab));
        assert_eq!(from_virtual_key(VK_TAB, CTRL_SHIFT), Some(Shortcut::PrevTab));
        assert_eq!(from_virtual_key('3' as u32, CTRL), Some(Shortcut::SelectTab(2)));
        assert_eq!(from_virtual_key('9' as u32, CTRL), Some(Shortcut::LastTab));
        assert_eq!(from_virtual_key(VK_F6, Modifiers::default()), Some(Shortcut::FocusAddress));
        assert_eq!(from_virtual_key('D' as u32, Modifiers { alt: true, ..Default::default() }), Some(Shortcut::FocusAddress));
    }

    #[test]
    fn devtools_keys_are_taken_from_the_page() {
        // Otherwise WebView2 opens its own floating window instead of the docked one.
        assert_eq!(from_virtual_key(VK_F12, Modifiers::default()), Some(Shortcut::ToggleDevtools));
        assert_eq!(from_virtual_key('I' as u32, CTRL_SHIFT), Some(Shortcut::ToggleDevtools));
        assert_eq!(from_virtual_key('I' as u32, CTRL), None, "italic in editors");
    }

    #[test]
    fn full_screen() {
        let none = Modifiers::default();
        assert_eq!(from_virtual_key(VK_F11, none), Some(Shortcut::ToggleFullScreen));
        assert_eq!(from_name("fullscreen"), Some(Shortcut::ToggleFullScreen));

        // Esc only becomes a shortcut while the browser is full screen.
        assert_eq!(from_virtual_key(VK_ESCAPE, none), None);
        assert_eq!(from_virtual_key_in_fullscreen(VK_ESCAPE, none), Some(Shortcut::ExitFullScreen));
        assert_eq!(from_virtual_key_in_fullscreen(VK_ESCAPE, CTRL_SHIFT), None, "the page keeps Ctrl+Shift+Esc");
        // Everything else works the same either way.
        assert_eq!(from_virtual_key_in_fullscreen('T' as u32, CTRL), Some(Shortcut::NewTab));
    }

    #[test]
    fn ctrl_0_resets_zoom() {
        // WebView2 ignores Ctrl+0, which would leave a site's remembered zoom with no quick way back.
        assert_eq!(from_virtual_key('0' as u32, CTRL), Some(Shortcut::ResetZoom));
        assert_eq!(from_virtual_key(VK_NUMPAD0, CTRL), Some(Shortcut::ResetZoom));
        assert_eq!(from_virtual_key(0xBB, CTRL), None, "Ctrl+= stays with WebView2");
    }

    #[test]
    fn page_shortcuts_are_left_alone() {
        assert_eq!(from_virtual_key('F' as u32, CTRL), None, "find in page");
        assert_eq!(from_virtual_key('C' as u32, CTRL), None, "copy");
        assert_eq!(from_virtual_key('T' as u32, Modifiers::default()), None, "typing");
    }

    #[test]
    fn names() {
        assert_eq!(from_name("new-tab"), Some(Shortcut::NewTab));
        assert_eq!(from_name("tab-1"), Some(Shortcut::SelectTab(0)));
        assert_eq!(from_name("tab-0"), None);
        assert_eq!(from_name("nope"), None);
    }
}
