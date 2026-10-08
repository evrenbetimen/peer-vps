//! CPU pinning for hypervisor processes.
//!
//! Pinning the VMM process before its vCPU threads exist makes every thread
//! it spawns inherit the affinity. Linux only: macOS has no hard affinity API
//! and Windows pinning is left to the OS scheduler for now.

/// Pin process `pid` to `cores`; logs and carries on unpinned on failure.
#[cfg(target_os = "linux")]
pub fn pin(pid: u32, cores: &[u32]) {
    if cores.is_empty() {
        return;
    }
    let mut set = nix::sched::CpuSet::new();
    for &c in cores {
        if let Err(e) = set.set(c as usize) {
            tracing::warn!(core = c, error = %e, "cannot pin to core");
            return;
        }
    }
    let pid = nix::unistd::Pid::from_raw(pid as i32);
    if let Err(e) = nix::sched::sched_setaffinity(pid, &set) {
        tracing::warn!(?cores, error = %e, "sched_setaffinity failed; vm runs unpinned");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn pin(_pid: u32, _cores: &[u32]) {}
