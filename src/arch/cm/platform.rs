// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Cortex-M implementation of the scheduler platform traits.

use core::ffi::c_void;

use cortex_m::peripheral::{DCB, DWT, SCB, SYST};

use crate::arch::platform::{
    ContextSwitchPort, CriticalSectionPort, CycleCounterPort, InitialThreadContext,
    SchedulerTimerPort, ThreadStackPort,
};
use crate::clock::ticks_per_ms;
use crate::thread::{ThreadEntry, ThreadHandle};

/// Hardware-backed Cortex-M platform.
pub struct CortexMPlatform;

const SCHEDULER_TIMER_RELOAD_BITS: u32 = 24;
const SCHEDULER_TIMER_RELOAD_MIN: u32 = 1;
const SCHEDULER_TIMER_RELOAD_MAX: u32 = (1 << SCHEDULER_TIMER_RELOAD_BITS) - 1;

const THREAD_STACK_ALIGNMENT: usize = 8;
const THREAD_INITIAL_FRAME_WORDS: usize = 16;

const INITIAL_XPSR_THUMB_STATE: u32 = 0x0100_0000;
const EXC_RETURN_THREAD_MODE_PSP: u32 = 0xFFFF_FFFD;

static mut DWT_ELAPSE_START_CYCLE: u32 = 0;

impl ThreadStackPort for CortexMPlatform {
    const STACK_ALIGNMENT: usize = THREAD_STACK_ALIGNMENT;
    const INITIAL_FRAME_WORDS: usize = THREAD_INITIAL_FRAME_WORDS;

    unsafe fn init_thread_stack(
        sp: *mut u32,
        entry: ThreadEntry,
        arg: *mut c_void,
    ) -> InitialThreadContext {
        unsafe { init_cortex_m_thread_stack(sp, entry, arg) }
    }
}

unsafe fn init_cortex_m_thread_stack(
    mut sp: *mut u32,
    entry: ThreadEntry,
    arg: *mut c_void,
) -> InitialThreadContext {
    unsafe {
        sp = ((sp as usize) & !(THREAD_STACK_ALIGNMENT - 1)) as *mut u32;

        sp = sp.sub(1);
        *sp = INITIAL_XPSR_THUMB_STATE;

        sp = sp.sub(1);
        *sp = entry as usize as u32;

        sp = sp.sub(1);
        *sp = EXC_RETURN_THREAD_MODE_PSP;

        sp = sp.sub(1);
        *sp = 0;

        for _ in 0..3 {
            sp = sp.sub(1);
            *sp = 0;
        }

        sp = sp.sub(1);
        *sp = arg as usize as u32;

        for _ in 0..8 {
            sp = sp.sub(1);
            *sp = 0;
        }

        InitialThreadContext {
            sp: sp as u32,
            exc_return: EXC_RETURN_THREAD_MODE_PSP,
        }
    }
}

/// Run `f` while maskable interrupts are disabled.
impl CriticalSectionPort for CortexMPlatform {
    fn critical_section<R, F>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        cortex_m::interrupt::free(|_| f())
    }
}

impl ContextSwitchPort for CortexMPlatform {
    fn request_context_switch() {
        SCB::set_pendsv();
    }

    unsafe fn spawn_main_thread(thread: ThreadHandle) -> ! {
        unsafe { super::ctx_switch::spawn_main_thread(thread) }
    }
}

impl SchedulerTimerPort for CortexMPlatform {
    const RELOAD_BITS: u32 = SCHEDULER_TIMER_RELOAD_BITS;
    const RELOAD_MIN: u32 = SCHEDULER_TIMER_RELOAD_MIN;
    const RELOAD_MAX: u32 = SCHEDULER_TIMER_RELOAD_MAX;

    fn reload() -> Option<u32> {
        Some(SYST::get_reload())
    }

    fn current() -> Option<u32> {
        Some(SYST::get_current())
    }

    fn program_reload(reload: u32) -> bool {
        unsafe {
            (*SYST::PTR).rvr.write(reload);
            (*SYST::PTR).cvr.write(0);
        }

        true
    }
}

fn dwt_elapse_start_cycle() -> u32 {
    unsafe { core::ptr::read_volatile(&raw const DWT_ELAPSE_START_CYCLE) }
}

fn msec_from_cycles(cycles: u32) -> u32 {
    let ticks = ticks_per_ms();

    if ticks == 0 {
        return 0;
    }

    cycles / ticks
}

/// Enable the DWT cycle counter and reset the elapsed-time baseline.
///
/// Returns `false` when the Cortex-M implementation does not provide a DWT
/// cycle counter or when enabling it did not take effect.
impl CycleCounterPort for CortexMPlatform {
    fn init_dwt_cycle_counter(dcb: &mut DCB, dwt: &mut DWT) -> bool {
        <Self as CriticalSectionPort>::critical_section(|| unsafe {
            if !DWT::has_cycle_counter() {
                return false;
            }

            dcb.enable_trace();
            dwt.set_cycle_count(0);
            dwt.enable_cycle_counter();

            if !DWT::cycle_counter_enabled() {
                return false;
            }

            core::ptr::write_volatile(&raw mut DWT_ELAPSE_START_CYCLE, DWT::cycle_count());
            true
        })
    }

    fn cycle_count() -> u32 {
        DWT::cycle_count()
    }

    fn reset_elapse_counter() {
        <Self as CriticalSectionPort>::critical_section(|| unsafe {
            core::ptr::write_volatile(&raw mut DWT_ELAPSE_START_CYCLE, DWT::cycle_count());
        });
    }

    fn elapse_cycles() -> u32 {
        DWT::cycle_count().wrapping_sub(dwt_elapse_start_cycle())
    }

    fn elapse_msec() -> u32 {
        msec_from_cycles(Self::elapse_cycles())
    }

    fn elapse_msec_since(start_cycle: u32) -> u32 {
        msec_from_cycles(DWT::cycle_count().wrapping_sub(start_cycle))
    }
}

/// Whether PendSV can switch the current thread after a blocking operation.
/// Read this before entering the operation's own critical section.
pub(crate) fn blocking_context_available() -> bool {
    let enabled = SCB::vect_active() == cortex_m::peripheral::scb::VectActive::ThreadMode
        && cortex_m::register::primask::read().is_active();
    #[cfg(target_has_atomic = "ptr")]
    let enabled = enabled
        && cortex_m::register::basepri::read() == 0
        && cortex_m::register::faultmask::read().is_active();
    enabled
}
