//! Measures how much memory the whole browser uses: this process plus every process it
//! started (the WebView2 browser, renderer, GPU and utility processes).

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub bytes: u64,
    pub processes: usize,
}

/// Private working set (what Task Manager shows as "Memory") summed over the process tree.
pub fn browser_usage() -> Usage {
    let pids = process_tree(std::process::id());
    Usage { bytes: pids.iter().map(|&pid| private_memory(pid)).sum(), processes: pids.len() }
}

fn process_tree(root: u32) -> Vec<u32> {
    let mut pairs: Vec<(u32, u32)> = Vec::with_capacity(256);
    unsafe {
        if let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snapshot);
        }
    }
    descendants(root, &pairs, started)
}

/// `root` plus every process descended from it. A child only counts if it started after its
/// parent: Windows reuses PIDs, and a parent PID is never updated when that parent exits. Seen
/// on a VM: a WebView2 process got smss's old PID, which made csrss, services and every svchost
/// look like ours.
fn descendants(root: u32, pairs: &[(u32, u32)], started: impl Fn(u32) -> Option<u64>) -> Vec<u32> {
    let mut tree = vec![root];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        let parent_started = started(parent);
        for &(pid, ppid) in pairs {
            if ppid == parent
                && pid != parent
                && pid != 0
                && !tree.contains(&pid)
                && matches!((parent_started, started(pid)), (Some(p), Some(c)) if c >= p)
            {
                tree.push(pid);
            }
        }
        i += 1;
    }
    tree
}

/// Process creation time as a FILETIME tick count, or `None` if the process can't be opened.
fn started(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user).is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

fn private_memory(pid: u32) -> u64 {
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, false, pid)
            .or_else(|_| OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid))
        {
            Ok(h) => h,
            Err(_) => return 0,
        };
        // EX2 (private working set) needs a recent Windows 10; fall back to private bytes.
        let mut ex2 = PROCESS_MEMORY_COUNTERS_EX2::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
        let bytes = if GetProcessMemoryInfo(handle, &mut ex2 as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size).is_ok()
            && ex2.PrivateWorkingSetSize > 0
        {
            ex2.PrivateWorkingSetSize
        } else {
            let mut ex = PROCESS_MEMORY_COUNTERS_EX::default();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            if GetProcessMemoryInfo(handle, &mut ex as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size).is_ok() {
                ex.PrivateUsage
            } else {
                0
            }
        };
        let _ = CloseHandle(handle);
        bytes as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reused_parent_pid_does_not_adopt_older_processes() {
        // 10 = us, 20 = our WebView2 child, which got the PID of a long-dead parent of 30;
        // 30's own child 40 must not come along either. 50 is a real child of 20.
        let pairs = [(10, 1), (20, 10), (30, 20), (40, 30), (50, 20)];
        let started = |pid: u32| Some(match pid {
            30 | 40 => 5,
            10 => 100,
            20 => 110,
            50 => 120,
            _ => return None,
        });
        assert_eq!(descendants(10, &pairs, started), vec![10, 20, 50]);
    }

    #[test]
    fn processes_we_cannot_open_are_left_out() {
        let pairs = [(10, 1), (20, 10)];
        assert_eq!(descendants(10, &pairs, |pid| (pid == 10).then_some(100)), vec![10]);
    }
}
