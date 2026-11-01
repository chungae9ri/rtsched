// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Host test implementation of the scheduler platform traits.
//!
//! The host serializes scheduler state and models thread storage for native
//! tests. It does not execute scheduler threads or provide hardware timers.

use core::ffi::c_void;

use cortex_m::peripheral::{DCB, DWT};

use crate::arch::platform::{
    ContextSwitchPort, CriticalSectionPort, CycleCounterPort, InitialThreadContext,
    SchedulerTimerPort, ThreadStackPort,
};
use crate::thread::{ThreadEntry, ThreadHandle};

/// Host platform used by native tests and documentation builds.
pub struct HostPlatform;

impl ThreadStackPort for HostPlatform {
    const STACK_ALIGNMENT: usize = 8;
    const INITIAL_FRAME_WORDS: usize = 16;

    unsafe fn init_thread_stack(
        sp: *mut u32,
        entry: ThreadEntry,
        arg: *mut c_void,
    ) -> InitialThreadContext {
        // Model the current 32-bit thread frame for scheduler tests. The host
        // never executes it, but retains its layout and restore token so native
        // tests exercise the same thread initialization as the embedded port.
        unsafe {
            let sp = ((sp as usize) & !(Self::STACK_ALIGNMENT - 1)) as *mut u32;
            let frame = sp.sub(Self::INITIAL_FRAME_WORDS);
            core::ptr::write_bytes(frame, 0, Self::INITIAL_FRAME_WORDS);
            frame.add(8).write(arg as usize as u32);
            frame.add(13).write(0xFFFF_FFFD);
            frame.add(14).write(entry as usize as u32);
            frame.add(15).write(0x0100_0000);

            InitialThreadContext {
                sp: frame as usize as u32,
                exc_return: 0xFFFF_FFFD,
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static HOST_CRITICAL_SECTION_DEPTH: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
static HOST_CRITICAL_SECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
struct HostCriticalSectionDepth;

#[cfg(test)]
impl Drop for HostCriticalSectionDepth {
    fn drop(&mut self) {
        HOST_CRITICAL_SECTION_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
    }
}

#[cfg(test)]
fn host_critical_section<R, F>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let nested = HOST_CRITICAL_SECTION_DEPTH.with(|depth| {
        let current = depth.get();
        depth.set(current + 1);
        current > 0
    });
    let _depth = HostCriticalSectionDepth;

    if nested {
        f()
    } else {
        let _lock = HOST_CRITICAL_SECTION_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f()
    }
}

#[cfg(not(test))]
fn host_critical_section<R, F>(f: F) -> R
where
    F: FnOnce() -> R,
{
    crate::test_support::critical_section(f)
}

impl CriticalSectionPort for HostPlatform {
    fn critical_section<R, F>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        host_critical_section(f)
    }
}

impl ContextSwitchPort for HostPlatform {
    fn request_context_switch() {}

    unsafe fn spawn_main_thread(_thread: ThreadHandle) -> ! {
        panic!("spawn_main_thread is only available on Cortex-M targets")
    }
}

impl SchedulerTimerPort for HostPlatform {
    const RELOAD_BITS: u32 = 24;
    const RELOAD_MIN: u32 = 1;
    const RELOAD_MAX: u32 = (1 << Self::RELOAD_BITS) - 1;

    fn reload() -> Option<u32> {
        None
    }

    fn current() -> Option<u32> {
        None
    }

    fn program_reload(_reload: u32) -> bool {
        false
    }
}

impl CycleCounterPort for HostPlatform {
    fn init_dwt_cycle_counter(_dcb: &mut DCB, _dwt: &mut DWT) -> bool {
        false
    }

    fn cycle_count() -> u32 {
        0
    }

    fn reset_elapse_counter() {}

    fn elapse_cycles() -> u32 {
        0
    }

    fn elapse_msec() -> u32 {
        0
    }

    fn elapse_msec_since(_start_cycle: u32) -> u32 {
        0
    }
}

/// Native tests cannot park a scheduler thread through a hardware context switch.
pub(crate) fn blocking_context_available() -> bool {
    false
}
