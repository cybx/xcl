//! Physical pages mapped into the title's own reservations, committed on first use.
//!
//! A title maps its physical pages wherever it reserved address space and places resources
//! there – render targets and depth buffers too, which on the PC live in video memory
//! (`d3d12x::vram`); the CPU never touches those pages. Committed at map time they doubled
//! that memory (a retail game: 5.4 GB of mapped pages next to its video-memory targets, past
//! the system's commit). So a page whose contents are still zero is mapped as reserved address
//! space only: the CPU's first access faults, the page is committed (zeros, as the page holds)
//! and the access goes on. Kernel calls do not fault in the title – they fail – so the buffers
//! of the file calls XCL routes, and DirectStorage's destinations, are committed first
//! (`ensure`).

use std::collections::BTreeSet;
use std::ffi::c_void;
use std::sync::{Once, RwLock};

use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
use windows_sys::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, EXCEPTION_POINTERS};
use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_READWRITE};

const PAGE: usize = 0x1_0000;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

/// pages (64 KiB) mapped but not committed yet
static LAZY: RwLock<BTreeSet<usize>> = RwLock::new(BTreeSet::new());

fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    });
}

/// Commit `page` if it is a lazy one (true: it was). The page leaves the lazy set only once it
/// is committed, under the lock: a thread that faults on it meanwhile waits for the commit.
unsafe fn commit(page: usize) -> bool {
    let mut g = LAZY.write().unwrap_or_else(|e| e.into_inner());
    if !g.contains(&page) {
        return false;
    }
    if VirtualAlloc(page as *const c_void, PAGE, MEM_COMMIT, PAGE_READWRITE).is_null() {
        return false;
    }
    g.remove(&page);
    true
}

/// Whether `p` is committed read-write now.
unsafe fn committed(p: usize) -> bool {
    let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
    VirtualQuery(p as *const c_void, &mut m, std::mem::size_of_val(&m)) != 0 && m.State == MEM_COMMIT && m.Protect == PAGE_READWRITE
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    let r = &*(*info).ExceptionRecord;
    // (reads and writes; an execute fault is not ours)
    if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 || r.ExceptionInformation[0] > 1 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let at = r.ExceptionInformation[1];
    let page = at & !(PAGE - 1);
    let lazy = LAZY.read().unwrap_or_else(|e| e.into_inner()).contains(&page);
    if lazy && commit(page) {
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    // Another thread committed the page between this thread's fault and here (its first use,
    // or `ensure`): the access goes on. Without this the second of two threads touching a new
    // page at once ended the title (a retail game, loading: its streaming threads, write faults
    // on committed read-write pages).
    if committed(at) {
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// `page` (reserved, 64 KiB aligned) is mapped: committed on first use.
pub(crate) fn mark(page: usize) {
    install();
    LAZY.write().unwrap_or_else(|e| e.into_inner()).insert(page);
}

/// `page` is unmapped: true if it was never used (nothing to keep of it).
pub(super) fn unmark(page: usize) -> bool {
    LAZY.write().unwrap_or_else(|e| e.into_inner()).remove(&page)
}

/// The lazy pages of `[p, p + len)` committed – memory a kernel call or XCL writes.
pub fn ensure(p: usize, len: usize) {
    if len == 0 {
        return;
    }
    let pages: Vec<usize> = {
        let g = LAZY.read().unwrap_or_else(|e| e.into_inner());
        if g.is_empty() {
            return;
        }
        g.range(p & !(PAGE - 1)..p.saturating_add(len)).copied().collect()
    };
    for page in pages {
        unsafe { commit(page) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE, MEM_RESERVE};

    /// A lazy page reads as zeros on first touch, keeps what is written; `ensure` commits it
    /// without a fault.
    #[test]
    fn a_lazy_page_is_committed_on_first_use() {
        unsafe {
            let r = VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE, PAGE_READWRITE) as usize;
            mark(r);
            mark(r + PAGE);
            assert_eq!(std::ptr::read_volatile((r + 8) as *const u32), 0);
            std::ptr::write_volatile((r + 8) as *mut u32, 7);
            assert_eq!(std::ptr::read_volatile((r + 8) as *const u32), 7);
            assert!(!unmark(r), "used: no longer lazy");
            ensure(r + PAGE + 4, 4);
            assert!(!LAZY.read().unwrap().contains(&(r + PAGE)));
            assert!(crate::gpu::shader::readable(r + PAGE, 4));
            VirtualFree(r as *mut c_void, 0, MEM_RELEASE);
        }
    }

    /// Threads touching the same new lazy pages at once all go on (the ones that fault while
    /// another commits the page too).
    #[test]
    fn threads_touching_a_new_page_at_once_all_go_on() {
        const PAGES: usize = 64;
        const THREADS: usize = 8;
        unsafe {
            let r = VirtualAlloc(std::ptr::null(), PAGES * PAGE, MEM_RESERVE, PAGE_READWRITE) as usize;
            for i in 0..PAGES {
                mark(r + i * PAGE);
            }
            let start = std::sync::Barrier::new(THREADS);
            std::thread::scope(|s| {
                for t in 0..THREADS {
                    let start = &start;
                    s.spawn(move || {
                        start.wait();
                        for i in 0..PAGES {
                            std::ptr::write_volatile((r + i * PAGE + t * 8) as *mut u64, (i * THREADS + t) as u64);
                        }
                    });
                }
            });
            for i in 0..PAGES {
                for t in 0..THREADS {
                    assert_eq!(std::ptr::read_volatile((r + i * PAGE + t * 8) as *const u64), (i * THREADS + t) as u64);
                }
            }
            VirtualFree(r as *mut c_void, 0, MEM_RELEASE);
        }
    }
}
