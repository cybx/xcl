//! Opt-in code patches for known title builds (`XCL_NO_TITLE_WATCHDOG=1`).
//!
//! Only offsets and short byte signatures live here, nothing of the title itself. A patch is
//! written only where the image holds exactly the bytes it replaces, and a build's patches
//! apply all or none.

use crate::modules::MappedModule;

/// One code patch: `rva` must hold exactly `expect` before `new` is written.
struct Patch {
    what: &'static str,
    rva: usize,
    expect: &'static [u8],
    new: &'static [u8],
}

/// A newer retail title (1.0.2.1) ends itself on purpose when one of the threads its watchdog
/// thread follows has not checked in for some 30 one-second ticks: the fatal handler writes a
/// tag to address 0 (`mov dword ptr [0], imm32`), and right after the watchdog's call of it a
/// fallback does the same (`mov dword ptr [0], 0; int3`). Translated pipelines still being
/// built stall those threads far longer than the console ever does (#846). Jumping over the
/// call and the fallback returns the watchdog loop to its normal path; the second patch makes
/// the handler harmless when another check calls it.
const WATCHDOG: &[Patch] = &[
    Patch {
        what: "watchdog: skip the call and the fallback fault",
        rva: 0x2215c27,
        // mov rax, [rip + disp32] (the handler pointer)
        expect: &[0x48, 0x8B, 0x05, 0x1A, 0x07, 0xFA, 0x04],
        // jmp +0x18 (to the loop's normal continuation), then NOPs
        new: &[0xEB, 0x18, 0x90, 0x90, 0x90, 0x90, 0x90],
    },
    Patch {
        what: "watchdog: the handler's deliberate fault",
        rva: 0x23cf683,
        // mov dword ptr [0], 0x4d8bd421
        expect: &[0xC7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xD4, 0x8B, 0x4D],
        // an eleven-byte NOP
        new: &[0x66, 0x66, 0x2E, 0x0F, 0x1F, 0x84, 0, 0, 0, 0, 0],
    },
];

/// With `XCL_NO_TITLE_WATCHDOG` set: disarms the title watchdog of a build whose bytes match
/// (all of its patches or none). Returns how many patches were written.
///
/// # Safety
/// `m` is the mapped main image and none of its code runs yet.
pub unsafe fn apply(m: &MappedModule) -> usize {
    if std::env::var_os("XCL_NO_TITLE_WATCHDOG").is_none_or(|v| v == "0") {
        return 0;
    }
    let n = apply_all(m.base, m.size, WATCHDOG);
    if n == 0 {
        crate::xlog_warn!("XCL_NO_TITLE_WATCHDOG: not a known build (its bytes differ) – nothing patched");
    }
    n
}

/// Writes all of `patches` if each one's bytes are as expected inside `[base, base + size)`,
/// else none. Returns how many were written.
unsafe fn apply_all(base: usize, size: usize, patches: &[Patch]) -> usize {
    let fits = |p: &Patch| p.new.len() == p.expect.len() && p.rva.checked_add(p.expect.len()).is_some_and(|end| end <= size);
    // SAFETY: inside the image (checked by `fits`), which the caller says is mapped.
    let matches = |p: &Patch| fits(p) && unsafe { std::slice::from_raw_parts((base + p.rva) as *const u8, p.expect.len()) } == p.expect;
    if !patches.iter().all(matches) {
        return 0;
    }
    patches.iter().filter(|p| unsafe { write(base + p.rva, p) }).count()
}

unsafe fn write(at: usize, p: &Patch) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut old = 0;
    if VirtualProtect(at as _, p.new.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
        crate::xlog_warn!("{}: VirtualProtect failed – not patched", p.what);
        return false;
    }
    std::ptr::copy_nonoverlapping(p.new.as_ptr(), at as *mut u8, p.new.len());
    VirtualProtect(at as _, p.new.len(), old, &mut old);
    FlushInstructionCache(GetCurrentProcess(), at as _, p.new.len());
    crate::xlog_info!("title patch: {}", p.what);
    true
}

#[cfg(test)]
mod tests {
    use super::{apply_all, Patch};

    const P: &[Patch] = &[
        Patch { what: "a", rva: 2, expect: &[1, 2], new: &[9, 9] },
        Patch { what: "b", rva: 6, expect: &[3], new: &[8] },
    ];

    #[test]
    fn a_build_is_patched_whole_or_not_at_all() {
        let mut image = vec![0u8, 0, 1, 2, 0, 0, 3, 0];
        assert_eq!(unsafe { apply_all(image.as_mut_ptr() as usize, image.len(), P) }, 2);
        assert_eq!(image, [0, 0, 9, 9, 0, 0, 8, 0]);
        // Another build: one patch's bytes differ, so the other is left alone too.
        let mut other = vec![0u8, 0, 1, 2, 0, 0, 4, 0];
        assert_eq!(unsafe { apply_all(other.as_mut_ptr() as usize, other.len(), P) }, 0);
        assert_eq!(other, [0, 0, 1, 2, 0, 0, 4, 0]);
        // A patch past the end of a smaller image is not read.
        let mut short = vec![0u8, 0, 1, 2, 0, 0];
        assert_eq!(unsafe { apply_all(short.as_mut_ptr() as usize, short.len(), P) }, 0);
    }
}
