//! Process resource usage, shown in the status bar.

use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// Private (committed) memory of this process, in bytes.
pub fn private_bytes() -> u64 {
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            c.cb,
        )
        .is_ok()
        {
            c.PrivateUsage as u64
        } else {
            0
        }
    }
}

/// Working set (physical memory in use), in bytes.
pub fn working_set() -> u64 {
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb).is_ok() {
            c.WorkingSetSize as u64
        } else {
            0
        }
    }
}
