//! Read-only access to another process address space.
//!
//! Everything here is `PROCESS_VM_READ` + `PROCESS_QUERY_INFORMATION` only.
//! Nothing writes, injects, or hooks — the target process is never modified.

#[cfg(not(windows))]
compile_error!("wf-vendor-probe only supports Windows");

use std::ffi::c_void;
use std::mem;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{
    VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE, PAGE_EXECUTE_READ,
    PAGE_GUARD, PAGE_NOACCESS, PAGE_WRITECOMBINE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};

/// One entry from the VirtualQueryEx walk.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub base: usize,
    pub size: usize,
    pub state: u32,
    pub protect: u32,
    pub kind: u32,
}

impl Region {
    pub fn end(&self) -> usize {
        self.base.saturating_add(self.size)
    }

    /// Committed, readable, and not a guard page.
    fn readable(&self) -> bool {
        self.state == MEM_COMMIT
            && self.size > 0
            && self.protect & PAGE_NOACCESS == 0
            && self.protect & PAGE_GUARD == 0
    }

    /// Heap-style data pages. Pure-code pages are excluded because an API
    /// response never lands there.
    pub fn is_data(&self) -> bool {
        self.readable() && self.protect != PAGE_EXECUTE && self.protect != PAGE_EXECUTE_READ
    }

    /// Data pages plus read-only code pages. A wider net, used by `strings`
    /// because DLL const-string tables live under PAGE_EXECUTE_READ.
    pub fn is_data_or_rodata(&self) -> bool {
        self.readable() && self.protect != PAGE_EXECUTE
    }

    /// Write-combined memory — GPU staging buffers, not general heap.
    ///
    /// These regions are large and uncached, so reading them dominates the cost
    /// of a pass while never holding an API response. Skipped by default.
    pub fn is_write_combine(&self) -> bool {
        self.protect & PAGE_WRITECOMBINE != 0
    }
}

/// An opened process handle. Closed on drop.
pub struct Proc {
    handle: HANDLE,
}

impl Drop for Proc {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Proc {
    pub fn open(pid: u32) -> Result<Proc, String> {
        let handle = unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION, 0, pid) };
        if handle == 0 {
            return Err(format!(
                "OpenProcess failed for pid {pid}. Anti-cheat may be denying read access, \
                 or the probe needs to run at the same privilege level as the game."
            ));
        }
        Ok(Proc { handle })
    }

    /// Full VirtualQueryEx walk of the user-mode address space.
    pub fn regions(&self) -> Vec<Region> {
        let mut out = Vec::new();
        let mut addr: usize = 0x10000;
        let mbi_size = mem::size_of::<MEMORY_BASIC_INFORMATION>();
        loop {
            let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { mem::zeroed() };
            if unsafe { VirtualQueryEx(self.handle, addr as *const c_void, &mut mbi, mbi_size) } == 0
            {
                break;
            }
            let base = mbi.BaseAddress as usize;
            let end = base.saturating_add(mbi.RegionSize);
            // A region that fails to advance the cursor means the walk is done
            // (or the kernel returned something nonsensical) — stop either way.
            if end <= addr {
                break;
            }
            addr = end;
            out.push(Region {
                base,
                size: mbi.RegionSize,
                state: mbi.State,
                protect: mbi.Protect,
                kind: mbi.Type,
            });
        }
        out
    }

    /// Read up to `len` bytes. Returns the bytes actually transferred, which can
    /// be fewer than requested when the range straddles an unreadable page.
    pub fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        let mut n: usize = 0;
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                addr as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                len,
                &mut n,
            )
        };
        if ok == 0 || n == 0 {
            return None;
        }
        buf.truncate(n);
        Some(buf)
    }
}

/// Stitch a contiguous run of readable regions around `center`.
///
/// A JSON response larger than one allocation is split across several regions,
/// and the opening brace of the object can sit before the region the needle was
/// found in. This walks outward in both directions across regions that touch
/// end-to-end, returning `(buffer, base_address_of_buffer)`.
pub fn read_window(
    p: &Proc,
    regions: &[Region],
    center: usize,
    back_cap: usize,
    fwd_cap: usize,
) -> Option<(Vec<u8>, usize)> {
    let idx = regions
        .binary_search_by(|r| {
            if center < r.base {
                std::cmp::Ordering::Greater
            } else if center >= r.end() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()?;

    // Walk back over regions that are contiguous with the one that follows them.
    let mut first = idx;
    let mut back = 0usize;
    while first > 0 {
        let prev = regions[first - 1];
        if prev.end() != regions[first].base || !prev.is_data() || back + prev.size > back_cap {
            break;
        }
        back += prev.size;
        first -= 1;
    }

    let total_cap = back.saturating_add(fwd_cap);
    let mut buf: Vec<u8> = Vec::new();
    let base = regions[first].base;
    let mut want_end = base;
    let mut i = first;
    while i < regions.len() {
        let r = regions[i];
        if r.base != want_end || !r.is_data() {
            break;
        }
        let remaining = total_cap.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        let take = r.size.min(remaining);
        match p.read(r.base, take) {
            Some(chunk) => {
                let short = chunk.len() < take;
                buf.extend_from_slice(&chunk);
                // A short read means the rest of this region is inaccessible,
                // so the stitched run ends here.
                if short {
                    break;
                }
            }
            None => break,
        }
        want_end = r.base + take;
        if take < r.size {
            break;
        }
        i += 1;
    }

    if buf.is_empty() {
        None
    } else {
        Some((buf, base))
    }
}

/// Find the running Warframe client by executable name.
///
/// Uses a ToolHelp snapshot rather than OpenProcess so detection still works
/// when read access to the process itself is denied. Third-party tools are
/// often named warframe-something too, so the game's own executable names win
/// over any other prefix match.
pub fn find_pid(name_prefix: &str) -> Option<u32> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32 = mem::zeroed();
        entry.dwSize = mem::size_of::<PROCESSENTRY32>() as u32;

        let mut best: Option<(u8, u32)> = None;
        if Process32First(snapshot, &mut entry) != 0 {
            loop {
                let len = entry.szExeFile.iter().position(|&b| b == 0).unwrap_or(260);
                let exe = String::from_utf8_lossy(&entry.szExeFile[..len]).to_lowercase();
                if exe.starts_with(name_prefix)
                    && !["launcher", "companion", "probe", "overlay"].iter().any(|s| exe.contains(s))
                {
                    let rank = match exe.as_str() {
                        "warframe.x64.exe" => 0,
                        "warframe.exe" => 1,
                        _ => 2,
                    };
                    if best.map_or(true, |(r, _)| rank < r) {
                        best = Some((rank, entry.th32ProcessID));
                    }
                }
                if Process32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        best.map(|(_, pid)| pid)
    }
}
