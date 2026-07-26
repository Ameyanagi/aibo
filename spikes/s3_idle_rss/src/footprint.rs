//! Process footprint sampling, per the measurement rules in docs/plan.md §15.
//!
//! > Measure properly or don't claim: macOS `phys_footprint` (not RSS),
//! > Windows private working set **and** commit, GPU memory separately,
//! > p50 **and p95**, on a named hardware class.
//!
//! This module gives you `phys_footprint` on macOS and working-set + commit on
//! Windows, plus cumulative CPU time so the "idle CPU" row of §15 can be
//! checked from the same samples.

/// One footprint sample of the current process.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// macOS: `ri_phys_footprint`. Windows: `WorkingSetSize`. Bytes.
    pub footprint_bytes: u64,
    /// macOS: `ri_resident_size`. Windows: `PrivateUsage` (commit). Bytes.
    pub secondary_bytes: u64,
    /// Cumulative user CPU time in nanoseconds.
    pub user_ns: u64,
    /// Cumulative system CPU time in nanoseconds.
    pub system_ns: u64,
}

impl Sample {
    /// Total CPU time consumed so far, in nanoseconds.
    pub fn cpu_ns(&self) -> u64 {
        self.user_ns.saturating_add(self.system_ns)
    }
}

/// Label for [`Sample::footprint_bytes`] on this platform.
pub const PRIMARY_LABEL: &str = if cfg!(target_os = "macos") {
    "phys_footprint"
} else if cfg!(target_os = "windows") {
    "working_set"
} else {
    "unsupported"
};

/// Label for [`Sample::secondary_bytes`] on this platform.
pub const SECONDARY_LABEL: &str = if cfg!(target_os = "macos") {
    "resident_size"
} else if cfg!(target_os = "windows") {
    "commit(PrivateUsage)"
} else {
    "unsupported"
};

/// Take a footprint sample of the current process.
///
/// Returns [`Sample::default`] on platforms this spike does not implement.
pub fn sample() -> Sample {
    #[cfg(target_os = "macos")]
    {
        macos::sample()
    }
    #[cfg(target_os = "windows")]
    {
        windows_impl::sample()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Sample::default()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Sample;

    /// Read `rusage_info_v4` for the current process.
    ///
    /// # Why `unsafe`
    ///
    /// `proc_pid_rusage` is a C API with an out-parameter typed as
    /// `void *`; there is no safe Rust wrapper for `phys_footprint`, and
    /// `phys_footprint` is precisely the number §15 requires (RSS over-counts
    /// shared pages and under-counts IOKit/compressed memory, which is exactly
    /// where a wgpu surface lives). The unsafety is confined to the four lines
    /// below: a zeroed, correctly-typed, stack-allocated `rusage_info_v4` is
    /// passed by pointer, and the result is only read when the call returns 0.
    pub fn sample() -> Sample {
        // SAFETY: `ri` is a live, correctly-sized `rusage_info_v4` on our own
        // stack; `RUSAGE_INFO_V4` is the flavour constant that matches that
        // struct; `getpid()` is our own process, which always exists. The
        // struct is only read if the call reports success.
        unsafe {
            let mut ri: libc::rusage_info_v4 = std::mem::zeroed();
            let rc = libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V4,
                (&raw mut ri).cast::<libc::c_void>().cast(),
            );
            if rc != 0 {
                return Sample::default();
            }
            Sample {
                footprint_bytes: ri.ri_phys_footprint,
                secondary_bytes: ri.ri_resident_size,
                user_ns: ri.ri_user_time,
                system_ns: ri.ri_system_time,
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::Sample;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    /// Read working set + commit + CPU times for the current process.
    ///
    /// # Why `unsafe`
    ///
    /// Both `GetProcessMemoryInfo` and `GetProcessTimes` are raw Win32 calls
    /// with out-parameters. The unsafety is confined to this function: every
    /// out-parameter is a zeroed, correctly-sized stack local, and
    /// `GetCurrentProcess()` returns a pseudo-handle that never needs closing.
    ///
    /// SPIKE: S3 — this reports `WorkingSetSize`, which is **not** the
    /// "private working set" §15 asks for. Private working set needs
    /// `QueryWorkingSetEx` page-by-page accounting. Decide in P0 whether the
    /// difference matters enough to implement it; until then treat the Windows
    /// number here as an upper bound, not the budget figure.
    pub fn sample() -> Sample {
        // SAFETY: see the doc comment. All pointers are to live stack locals of
        // exactly the size declared in `cb` / the API contract.
        unsafe {
            let process = GetCurrentProcess();

            let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
            counters.cb = u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap_or(0);
            let mem_ok = GetProcessMemoryInfo(
                process,
                (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
            .is_ok();

            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            let times_ok = GetProcessTimes(
                process,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
            .is_ok();

            // FILETIME is in 100 ns units.
            let to_ns = |ft: FILETIME| -> u64 {
                let v = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
                v.saturating_mul(100)
            };

            Sample {
                footprint_bytes: if mem_ok { counters.WorkingSetSize as u64 } else { 0 },
                secondary_bytes: if mem_ok { counters.PrivateUsage as u64 } else { 0 },
                user_ns: if times_ok { to_ns(user) } else { 0 },
                system_ns: if times_ok { to_ns(kernel) } else { 0 },
            }
        }
    }
}
