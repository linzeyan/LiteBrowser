//! Memory policy: decides which background tabs get frozen or discarded.
//!
//! Pure logic so it can be unit tested; the app feeds it a snapshot of the tabs and executes
//! the returned actions.
//!
//! ```text
//!   Live ──(background ≥ suspend_after)──▶ Suspended ──(background ≥ discard_after)──▶ Discarded
//!     ▲                                                                                  │
//!     └───────────────────────── user switches back: WebView is recreated ◀──────────────┘
//! ```
//! On top of the timers:
//! - at most `max_live` tabs keep a WebView, and while the browser uses more than the budget the
//!   least recently used background tab is discarded — but only once it has been in the
//!   background for `grace`, so switching back and forth between tabs never reloads them;
//! - when the machine itself runs low on free memory, the least recently used background tab is
//!   discarded right away.

pub type TabId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Residency {
    /// WebView exists and runs (or is being created).
    Live,
    /// WebView exists but is frozen with TrySuspend.
    Suspended,
    /// No WebView; only URL and title are kept.
    Discarded,
}

#[derive(Clone, Debug)]
pub struct TabSnapshot {
    pub id: TabId,
    pub residency: Residency,
    pub active: bool,
    pub pinned: bool,
    /// Last time (ms) this tab was the active tab.
    pub last_active_ms: u64,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub max_live: usize,
    pub suspend_after_ms: u64,
    pub discard_after_ms: u64,
    pub budget_bytes: u64,
    /// Cap and budget only discard tabs that have been in the background at least this long.
    pub grace_ms: u64,
    /// Below this much free system memory, discard without waiting for the grace period.
    pub low_free_bytes: u64,
}

impl Policy {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            max_live: cfg.max_live_tabs.max(1),
            suspend_after_ms: cfg.suspend_after_secs * 1000,
            discard_after_ms: cfg.discard_after_mins * 60 * 1000,
            budget_bytes: cfg.memory_budget_mb * 1024 * 1024,
            grace_ms: cfg.discard_grace_mins * 60 * 1000,
            low_free_bytes: cfg.low_memory_free_mb * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryState {
    /// Everything the browser uses (this process and all WebView2 processes).
    pub browser_bytes: u64,
    /// Free physical memory of the machine.
    pub system_free_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Suspend(TabId),
    Discard(TabId),
}

pub fn plan(tabs: &[TabSnapshot], now_ms: u64, memory: Option<MemoryState>, policy: &Policy) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut discarded: Vec<TabId> = Vec::new();

    // Background tabs that hold a WebView, least recently used first.
    let mut background: Vec<&TabSnapshot> =
        tabs.iter().filter(|t| !t.active && t.residency != Residency::Discarded).collect();
    background.sort_by_key(|t| t.last_active_ms);

    let idle = |t: &TabSnapshot| now_ms.saturating_sub(t.last_active_ms);

    fn discard(t: &TabSnapshot, actions: &mut Vec<Action>, discarded: &mut Vec<TabId>) {
        actions.retain(|a| *a != Action::Suspend(t.id));
        actions.push(Action::Discard(t.id));
        discarded.push(t.id);
    }

    // 1. Timers.
    for t in &background {
        if !t.pinned && idle(t) >= policy.discard_after_ms {
            discard(t, &mut actions, &mut discarded);
        } else if t.residency == Residency::Live && idle(t) >= policy.suspend_after_ms {
            actions.push(Action::Suspend(t.id));
        }
    }

    // 2. Cap on tabs holding a WebView. Tabs still inside the grace period are frozen instead.
    let resident = tabs.iter().filter(|t| t.residency != Residency::Discarded).count() - discarded.len();
    let mut excess = resident.saturating_sub(policy.max_live);
    for t in &background {
        if excess == 0 {
            break;
        }
        if t.pinned || discarded.contains(&t.id) {
            continue;
        }
        if idle(t) >= policy.grace_ms {
            discard(t, &mut actions, &mut discarded);
        } else if t.residency == Residency::Live && !actions.contains(&Action::Suspend(t.id)) {
            actions.push(Action::Suspend(t.id));
        }
        excess -= 1;
    }

    // 3. Memory: one tab per round, so the next measurement can reflect it.
    if let Some(mem) = memory {
        let candidate = |respect_grace: bool| {
            background
                .iter()
                .find(|t| !t.pinned && !discarded.contains(&t.id) && (!respect_grace || idle(t) >= policy.grace_ms))
                .copied()
        };
        let pick = if mem.system_free_bytes > 0 && mem.system_free_bytes < policy.low_free_bytes {
            candidate(false)
        } else if mem.browser_bytes > policy.budget_bytes {
            candidate(true)
        } else {
            None
        };
        if let Some(t) = pick {
            discard(t, &mut actions, &mut discarded);
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use Residency::*;

    const SEC: u64 = 1000;
    const MIN: u64 = 60 * SEC;
    const MB: u64 = 1 << 20;

    fn policy() -> Policy {
        Policy {
            max_live: 2,
            suspend_after_ms: 30 * SEC,
            discard_after_ms: 15 * MIN,
            budget_bytes: 500 * MB,
            grace_ms: 5 * MIN,
            low_free_bytes: 250 * MB,
        }
    }

    fn tab(id: TabId, residency: Residency, active: bool, last_active_ms: u64) -> TabSnapshot {
        TabSnapshot { id, residency, active, pinned: false, last_active_ms }
    }

    fn browser(bytes: u64) -> Option<MemoryState> {
        Some(MemoryState { browser_bytes: bytes, system_free_bytes: 2048 * MB })
    }

    fn system_free(bytes: u64) -> Option<MemoryState> {
        Some(MemoryState { browser_bytes: 0, system_free_bytes: bytes })
    }

    #[test]
    fn active_tab_is_never_touched() {
        let tabs = [tab(1, Live, true, 0)];
        assert!(plan(&tabs, 100 * MIN, Some(MemoryState { browser_bytes: u64::MAX, system_free_bytes: 1 }), &policy())
            .is_empty());
    }

    #[test]
    fn background_tab_is_suspended_then_discarded() {
        let now = 100 * MIN;
        let tabs = [tab(1, Live, true, now), tab(2, Live, false, now - 10 * SEC)];
        assert!(plan(&tabs, now, None, &policy()).is_empty());

        let tabs = [tab(1, Live, true, now), tab(2, Live, false, now - 31 * SEC)];
        assert_eq!(plan(&tabs, now, None, &policy()), vec![Action::Suspend(2)]);

        let tabs = [tab(1, Live, true, now), tab(2, Suspended, false, now - 16 * MIN)];
        assert_eq!(plan(&tabs, now, None, &policy()), vec![Action::Discard(2)]);
    }

    #[test]
    fn suspended_tab_is_not_suspended_again() {
        let now = 100 * MIN;
        let tabs = [tab(1, Live, true, now), tab(2, Suspended, false, now - MIN)];
        assert!(plan(&tabs, now, None, &policy()).is_empty());
    }

    #[test]
    fn over_budget_does_not_discard_a_recently_used_tab() {
        // The reported bug: one big page over the budget made every switch reload the other tab.
        let now = 100 * MIN;
        let tabs = [tab(1, Live, true, now), tab(2, Live, false, now - 3 * SEC)];
        assert!(plan(&tabs, now, browser(900 * MB), &policy()).is_empty());

        let tabs = [tab(1, Live, true, now), tab(2, Suspended, false, now - 6 * MIN)];
        assert_eq!(plan(&tabs, now, browser(900 * MB), &policy()), vec![Action::Discard(2)]);
    }

    #[test]
    fn low_system_memory_ignores_grace() {
        let now = 100 * MIN;
        let tabs = [tab(1, Live, true, now), tab(2, Live, false, now - 3 * SEC), tab(3, Live, false, now - 2 * SEC)];
        assert_eq!(plan(&tabs, now, system_free(100 * MB), &Policy { max_live: 5, ..policy() }), vec![
            Action::Discard(2)
        ]);
        assert!(plan(&tabs, now, system_free(800 * MB), &Policy { max_live: 5, ..policy() }).is_empty());
    }

    #[test]
    fn live_cap_freezes_inside_grace_and_discards_after() {
        let now = 100 * MIN;
        let tabs = [
            tab(1, Live, true, now),
            tab(2, Live, false, now - 3 * SEC),
            tab(3, Live, false, now - 5 * SEC),
            tab(4, Discarded, false, now - 1),
        ];
        assert_eq!(plan(&tabs, now, None, &policy()), vec![Action::Suspend(3)]);

        let tabs = [tab(1, Live, true, now), tab(2, Live, false, now - 3 * SEC), tab(3, Suspended, false, now - 6 * MIN)];
        assert_eq!(plan(&tabs, now, None, &policy()), vec![Action::Discard(3)]);
    }

    #[test]
    fn pinned_tabs_are_frozen_but_never_discarded() {
        let now = 100 * MIN;
        let mut pinned = tab(2, Live, false, now - 60 * MIN);
        pinned.pinned = true;
        let tabs = [tab(1, Live, true, now), pinned, tab(3, Live, false, now - 10 * MIN)];
        assert_eq!(plan(&tabs, now, system_free(MB), &policy()), vec![Action::Suspend(2), Action::Discard(3)]);
    }

    #[test]
    fn no_duplicate_discards() {
        let now = 100 * MIN;
        let tabs =
            [tab(1, Live, true, now), tab(2, Suspended, false, now - 20 * MIN), tab(3, Live, false, now - 10 * MIN)];
        let actions = plan(&tabs, now, browser(u64::MAX), &Policy { max_live: 1, ..policy() });
        assert_eq!(actions, vec![Action::Discard(2), Action::Discard(3)]);
    }
}
