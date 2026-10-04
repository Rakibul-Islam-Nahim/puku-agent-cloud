//! How many microVMs this host can actually hold.
//!
//! Capacity used to be a fixed number an operator typed in
//! (`PUKU_CAPACITY_SLOTS`, default 8), which is wrong in both directions. On
//! a 62 GiB, 24-core box it left roughly three quarters of the machine idle
//! while sessions queued. On a small box it happily accepted eight 2 GiB
//! VMs onto 8 GiB of RAM, and the failure was not a queue but an OOM kill
//! partway through somebody's paid run.
//!
//! So the host decides. Two numbers, doing different jobs:
//!
//! * a **ceiling** from total RAM and cores -- the most VMs this machine
//!   could ever hold, which does not move;
//! * an **admission** figure from memory that is free *now*, which does.
//!
//! The reported capacity is `used + what still fits`, clamped to the
//! ceiling. That makes it self-limiting: as VMs fill the box the figure
//! falls to meet `used_slots`, the control plane stops assigning, and it
//! recovers on its own as sessions finish. Nobody has to predict the right
//! number, and a host under memory pressure refuses work instead of
//! accepting it and dying.

/// What controld puts on every session spec. Kept here because capacity is
/// only meaningful in terms of the VM size it is dividing by; if the spec
/// ever varies per session, this becomes a per-assignment calculation
/// rather than a constant.
pub const VM_CPUS: u32 = 2;
pub const VM_MEMORY_MIB: u64 = 2048;

/// What the machine has right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub cores: u32,
    pub mem_total_mib: u64,
    /// Memory the kernel believes is available without swapping. Counts
    /// reclaimable page cache, which `MemFree` does not -- on a box that has
    /// been pulling images, `MemFree` reads as near-zero and would refuse
    /// every session.
    pub mem_available_mib: u64,
}

/// Left for the host itself: the worker, dockerised control plane, page
/// cache, and the headroom an msb boot needs while a VM is starting.
const RESERVE_MIB: u64 = 4096;
const RESERVE_CORES: u32 = 2;

/// The most VMs this host could hold with nothing else running.
pub fn ceiling(host: &Host, cpus_per_vm: u32, mem_per_vm_mib: u64) -> u32 {
    if cpus_per_vm == 0 || mem_per_vm_mib == 0 {
        return 1;
    }
    let by_cores = host.cores.saturating_sub(RESERVE_CORES) / cpus_per_vm;
    let by_mem = host.mem_total_mib.saturating_sub(RESERVE_MIB) / mem_per_vm_mib;
    (by_cores.min(by_mem as u32)).max(1)
}

/// How many more VMs fit in the memory that is free right now.
pub fn fits_now(host: &Host, mem_per_vm_mib: u64) -> u32 {
    if mem_per_vm_mib == 0 {
        return 0;
    }
    (host.mem_available_mib.saturating_sub(RESERVE_MIB) / mem_per_vm_mib) as u32
}

/// The capacity to advertise: what is already running, plus what still
/// fits, never above the ceiling.
///
/// Reporting `used + fits` rather than a bare `fits` is what stops the
/// number collapsing to zero and stranding running sessions -- the control
/// plane reads capacity against `used_slots`, so a figure below `used`
/// would look like an over-committed host rather than a full one.
pub fn advertise(host: &Host, used: u32, cpus_per_vm: u32, mem_per_vm_mib: u64) -> u32 {
    let ceiling = ceiling(host, cpus_per_vm, mem_per_vm_mib);
    used.saturating_add(fits_now(host, mem_per_vm_mib)).min(ceiling).max(used.max(1))
}

/// Read the host's resources. Returns None where they cannot be read, so
/// the caller keeps whatever number it already had rather than guessing.
pub fn probe() -> Option<Host> {
    let cores = std::thread::available_parallelism().ok()?.get() as u32;
    let (total, available) = meminfo()?;
    Some(Host { cores, mem_total_mib: total, mem_available_mib: available })
}

/// Free and total space on the filesystem holding `path`, in MiB, or None
/// where it cannot be read. It is what VM disks and machine volumes are
/// written to, so it is what placement checks before sending one here.
// statvfs field widths differ by platform (block counts are u32 on macOS and
// u64 on Linux), so each widening `u64::from` is a no-op on some target.
#[allow(clippy::useless_conversion)]
pub fn disk(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is plain old data, so all-zero is a valid value to
    // hand the call, which only writes into it; `c` is a NUL-terminated path
    // that outlives the call.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let block = match u64::from(st.f_frsize) {
        0 => u64::from(st.f_bsize),
        n => n,
    };
    let mib = |blocks: u64| blocks.saturating_mul(block) / (1024 * 1024);
    Some((mib(u64::from(st.f_bavail)), mib(u64::from(st.f_blocks))))
}

#[cfg(target_os = "linux")]
fn meminfo() -> Option<(u64, u64)> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(not(target_os = "linux"))]
fn meminfo() -> Option<(u64, u64)> {
    None // the worker is Linux-only; elsewhere the configured value stands
}

/// `MemTotal` and `MemAvailable`, in MiB. Kept separate from the file read
/// so it can be tested on any platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kib| kib / 1024)
    };
    let total = field("MemTotal:")?;
    // Pre-3.14 kernels have no MemAvailable. MemFree understates badly, but
    // understating capacity is the safe direction.
    let available = field("MemAvailable:").or_else(|| field("MemFree:"))?;
    Some((total, available))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(cores: u32, total_gib: u64, avail_gib: u64) -> Host {
        Host {
            cores,
            mem_total_mib: total_gib * 1024,
            mem_available_mib: avail_gib * 1024,
        }
    }

    /// The deployment this was written against: 24 cores, 62 GiB, and a
    /// hand-typed limit of 8 that left most of the box idle.
    #[test]
    fn a_big_host_offers_far_more_than_the_old_fixed_eight() {
        let h = host(24, 62, 58);
        assert_eq!(ceiling(&h, 2, 2048), 11, "22 usable cores / 2 per VM");
        assert!(advertise(&h, 0, 2, 2048) > 8, "should beat the old hand-typed 8");
    }

    /// The other direction, which was the dangerous one: eight 2 GiB VMs
    /// were accepted onto an 8 GiB box and the kernel picked the loser.
    #[test]
    fn a_small_host_refuses_to_overcommit() {
        let h = host(4, 8, 7);
        assert_eq!(ceiling(&h, 2, 2048), 1, "8 GiB, minus reserve, holds one 2 GiB VM");
        assert!(advertise(&h, 0, 2, 2048) <= 2);
    }

    #[test]
    fn capacity_falls_as_memory_fills_and_recovers_when_it_frees() {
        let roomy = host(24, 62, 58);
        let tight = host(24, 62, 5);
        let full = advertise(&roomy, 0, 2, 2048);
        let squeezed = advertise(&tight, 6, 2, 2048);
        assert!(squeezed < full, "a host under pressure must advertise less");
        assert_eq!(advertise(&roomy, 6, 2, 2048), full, "and recover once it frees");
    }

    /// A figure below `used` reads as over-commitment rather than fullness,
    /// and would strand the sessions already running.
    #[test]
    fn capacity_never_drops_below_what_is_already_running() {
        let h = host(24, 62, 0);
        assert!(advertise(&h, 7, 2, 2048) >= 7);
    }

    #[test]
    fn nothing_is_ever_zero_capacity() {
        let h = host(1, 1, 0);
        assert!(ceiling(&h, 2, 2048) >= 1);
        assert!(advertise(&h, 0, 2, 2048) >= 1);
    }

    #[test]
    fn meminfo_is_parsed_in_mib() {
        let text = "MemTotal:       65536000 kB\nMemFree:         1000 kB\nMemAvailable:   32768000 kB\n";
        assert_eq!(parse_meminfo(text), Some((64000, 32000)));
    }

    /// MemFree on a box with a warm page cache reads near zero; falling back
    /// to it is safe, falling back to nothing is not.
    #[test]
    fn a_kernel_without_memavailable_falls_back_to_memfree() {
        let text = "MemTotal:       8192000 kB\nMemFree:        4096000 kB\n";
        assert_eq!(parse_meminfo(text), Some((8000, 4000)));
    }

    #[test]
    fn unparseable_meminfo_yields_nothing_rather_than_a_guess() {
        assert_eq!(parse_meminfo("nonsense\n"), None);
    }

    #[test]
    fn disk_space_is_read_where_the_path_exists() {
        let (free, total) = disk(std::path::Path::new("/")).expect("statvfs of /");
        assert!(total > 0 && free <= total, "{free} of {total} MiB");
        assert!(disk(std::path::Path::new("/nonexistent/puku")).is_none());
    }
}
