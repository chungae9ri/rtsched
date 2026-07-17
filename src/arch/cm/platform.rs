// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Cortex-M platform interface used by the scheduler core.
//!
//! Scheduler code outside `arch/cm` should use this module instead of calling
//! `cortex-m` APIs directly. Porting the scheduler to another Cortex-M timer
//! source should require changing this layer, not the ktimer queue logic.

use core::ffi::c_void;

#[cfg(not(target_arch = "arm"))]
use cortex_m::peripheral::{DCB, DWT};
#[cfg(target_arch = "arm")]
use cortex_m::peripheral::{DCB, DWT, SCB, SYST};

#[cfg(target_arch = "arm")]
use crate::clock::ticks_per_ms;
use crate::thread::{ThreadCtx, ThreadEntry};

pub(crate) const SCHEDULER_TIMER_RELOAD_BITS: u32 = 24;
pub(crate) const SCHEDULER_TIMER_RELOAD_MIN: u32 = 1;
pub(crate) const SCHEDULER_TIMER_RELOAD_MAX: u32 = (1 << SCHEDULER_TIMER_RELOAD_BITS) - 1;

pub(crate) const THREAD_STACK_ALIGNMENT: usize = 8;
pub(crate) const THREAD_INITIAL_FRAME_WORDS: usize = 16;

const INITIAL_XPSR_THUMB_STATE: u32 = 0x0100_0000;
const EXC_RETURN_THREAD_MODE_PSP: u32 = 0xFFFF_FFFD;

#[cfg(target_arch = "arm")]
static mut DWT_ELAPSE_START_CYCLE: u32 = 0;

pub(crate) struct InitialThreadContext {
    pub sp: u32,
    pub exc_return: u32,
}

/// Build the initial Cortex-M thread stack frame consumed by SVCall/PendSV.
///
/// After the context switch restores r4-r11 and sets PSP, exception return
/// consumes the standard hardware frame: r0, r1, r2, r3, r12, lr, pc, xPSR.
///
/// # Safety
///
/// `sp` must point to writable stack storage that is large enough for
/// `THREAD_INITIAL_FRAME_WORDS` 32-bit words.
pub(crate) unsafe fn init_thread_stack(
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
#[cfg(target_arch = "arm")]
pub(crate) fn critical_section<R>(f: impl FnOnce() -> R) -> R {
    cortex_m::interrupt::free(|_| f())
}

#[cfg(all(not(target_arch = "arm"), test))]
thread_local! {
    static HOST_CRITICAL_SECTION_DEPTH: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(all(not(target_arch = "arm"), test))]
static HOST_CRITICAL_SECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(not(target_arch = "arm"), test))]
struct HostCriticalSectionDepth;

#[cfg(all(not(target_arch = "arm"), test))]
impl Drop for HostCriticalSectionDepth {
    fn drop(&mut self) {
        HOST_CRITICAL_SECTION_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
    }
}

#[cfg(all(not(target_arch = "arm"), test))]
pub(crate) fn critical_section<R>(f: impl FnOnce() -> R) -> R {
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

#[cfg(all(not(target_arch = "arm"), not(test)))]
pub(crate) fn critical_section<R>(f: impl FnOnce() -> R) -> R {
    f()
}

#[cfg(target_arch = "arm")]
pub(crate) fn request_context_switch() {
    SCB::set_pendsv();
}

#[cfg(not(target_arch = "arm"))]
pub(crate) fn request_context_switch() {}

/// Spawn the first scheduler thread using the platform context-switch entry.
///
/// # Safety
///
/// See the Cortex-M context-switch backend for the required thread and stack
/// invariants.
pub unsafe fn spawn_main_thread(thread: *mut ThreadCtx) -> ! {
    unsafe { super::ctx_switch::spawn_main_thread(thread) }
}

/// Return the currently programmed scheduler-timer reload value.
#[cfg(target_arch = "arm")]
pub(crate) fn scheduler_timer_reload() -> Option<u32> {
    Some(SYST::get_reload())
}

#[cfg(not(target_arch = "arm"))]
pub(crate) fn scheduler_timer_reload() -> Option<u32> {
    None
}

/// Return the scheduler timer's current down-counter value.
#[cfg(target_arch = "arm")]
pub(crate) fn scheduler_timer_current() -> Option<u32> {
    Some(SYST::get_current())
}

#[cfg(not(target_arch = "arm"))]
pub(crate) fn scheduler_timer_current() -> Option<u32> {
    None
}

/// Program the scheduler timer for the next interval and clear elapsed state.
#[cfg(target_arch = "arm")]
pub(crate) fn program_scheduler_timer_reload(reload: u32) -> bool {
    unsafe {
        (*SYST::PTR).rvr.write(reload);
        (*SYST::PTR).cvr.write(0);
    }

    true
}

#[cfg(not(target_arch = "arm"))]
pub(crate) fn program_scheduler_timer_reload(_reload: u32) -> bool {
    false
}

#[cfg(target_arch = "arm")]
fn dwt_elapse_start_cycle() -> u32 {
    unsafe { core::ptr::read_volatile(&raw const DWT_ELAPSE_START_CYCLE) }
}

#[cfg(target_arch = "arm")]
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
#[cfg(target_arch = "arm")]
pub fn init_dwt_cycle_counter(dcb: &mut DCB, dwt: &mut DWT) -> bool {
    critical_section(|| unsafe {
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

#[cfg(not(target_arch = "arm"))]
pub fn init_dwt_cycle_counter(_dcb: &mut DCB, _dwt: &mut DWT) -> bool {
    false
}

#[cfg(target_arch = "arm")]
pub fn dwt_cycle_count() -> u32 {
    DWT::cycle_count()
}

#[cfg(not(target_arch = "arm"))]
pub fn dwt_cycle_count() -> u32 {
    0
}

/// Reset the baseline used by `get_elapse_cycles` and `get_elapse_msec`.
#[cfg(target_arch = "arm")]
pub fn reset_elapse_counter() {
    critical_section(|| unsafe {
        core::ptr::write_volatile(&raw mut DWT_ELAPSE_START_CYCLE, DWT::cycle_count());
    });
}

#[cfg(not(target_arch = "arm"))]
pub fn reset_elapse_counter() {}

/// Return elapsed DWT cycles since the last DWT elapsed-counter reset.
///
/// The subtraction is wrapping, so the value is valid for intervals shorter
/// than one full 32-bit DWT counter wrap.
#[cfg(target_arch = "arm")]
pub fn get_elapse_cycles() -> u32 {
    DWT::cycle_count().wrapping_sub(dwt_elapse_start_cycle())
}

#[cfg(not(target_arch = "arm"))]
pub fn get_elapse_cycles() -> u32 {
    0
}

/// Return elapsed milliseconds since the last DWT elapsed-counter reset.
///
/// `update_sys_clk_freq` must be called before using this conversion.
#[cfg(target_arch = "arm")]
pub fn get_elapse_msec() -> u32 {
    msec_from_cycles(get_elapse_cycles())
}

#[cfg(not(target_arch = "arm"))]
pub fn get_elapse_msec() -> u32 {
    0
}

#[cfg(target_arch = "arm")]
pub fn get_elapse_msec_since(start_cycle: u32) -> u32 {
    msec_from_cycles(DWT::cycle_count().wrapping_sub(start_cycle))
}

#[cfg(not(target_arch = "arm"))]
pub fn get_elapse_msec_since(_start_cycle: u32) -> u32 {
    0
}
