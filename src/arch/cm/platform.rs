// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Cortex-M platform interface used by the scheduler core.
//!
//! Scheduler code outside `arch/cm` should use this module instead of calling
//! `cortex-m` APIs directly. The module is split into small capability traits
//! so ports can replace a timer, cycle counter, critical-section primitive, or
//! context-switch backend without changing the ktimer queue logic.

use core::ffi::c_void;

use cortex_m::peripheral::{DCB, DWT};
#[cfg(target_arch = "arm")]
use cortex_m::peripheral::{SCB, SYST};

#[cfg(target_arch = "arm")]
use crate::clock::ticks_per_ms;
use crate::thread::{ThreadEntry, ThreadHandle};

const CORTEX_M_SCHEDULER_TIMER_RELOAD_BITS: u32 = 24;
const CORTEX_M_SCHEDULER_TIMER_RELOAD_MIN: u32 = 1;
const CORTEX_M_SCHEDULER_TIMER_RELOAD_MAX: u32 = (1 << CORTEX_M_SCHEDULER_TIMER_RELOAD_BITS) - 1;

const CORTEX_M_THREAD_STACK_ALIGNMENT: usize = 8;
const CORTEX_M_THREAD_INITIAL_FRAME_WORDS: usize = 16;

const INITIAL_XPSR_THUMB_STATE: u32 = 0x0100_0000;
const EXC_RETURN_THREAD_MODE_PSP: u32 = 0xFFFF_FFFD;

#[cfg(target_arch = "arm")]
static mut DWT_ELAPSE_START_CYCLE: u32 = 0;

/// Initial thread context produced by a platform stack initializer.
pub struct InitialThreadContext {
    pub sp: u32,
    pub exc_return: u32,
}

/// Builds the synthetic stack frame needed to start a scheduler thread.
pub trait ThreadStackPort {
    const STACK_ALIGNMENT: usize;
    const INITIAL_FRAME_WORDS: usize;

    /// # Safety
    ///
    /// `sp` must be the exclusive top-of-stack pointer for writable stack
    /// storage owned by the thread being initialized. At least
    /// `Self::INITIAL_FRAME_WORDS` 32-bit words below `sp` must be valid to
    /// write, and `sp` must satisfy `Self::STACK_ALIGNMENT`.
    ///
    /// The stack storage must outlive all scheduler use of the thread. `entry`
    /// must use the `ThreadEntry` ABI and must not return. `arg` is passed
    /// through unchanged; the caller must ensure it remains valid for whatever
    /// the entry function does with it.
    unsafe fn init_thread_stack(
        sp: *mut u32,
        entry: ThreadEntry,
        arg: *mut c_void,
    ) -> InitialThreadContext;
}

/// Protects scheduler globals from interrupt or test-thread interleaving.
pub trait CriticalSectionPort {
    fn critical_section<R, F>(f: F) -> R
    where
        F: FnOnce() -> R;
}

/// Provides the architecture context-switch entry points.
pub trait ContextSwitchPort {
    fn request_context_switch();

    /// # Safety
    ///
    /// `thread` must refer to a live thread created by a thread builder for
    /// the active platform. Its stack and thread storage must outlive all
    /// scheduler use, the scheduler queues must already be initialized, and no
    /// other scheduler thread may already be running.
    ///
    /// Call this only from privileged single-core startup code after the
    /// platform exception handlers and scheduler timer are configured enough
    /// for the context-switch backend to restore the thread.
    unsafe fn spawn_main_thread(thread: ThreadHandle) -> !;
}

/// Provides the reloadable timer used by the kernel timer queue.
pub trait SchedulerTimerPort {
    const RELOAD_BITS: u32;
    const RELOAD_MIN: u32;
    const RELOAD_MAX: u32;

    fn reload() -> Option<u32>;
    fn current() -> Option<u32>;
    fn program_reload(reload: u32) -> bool;
}

/// Provides a cycle counter used for elapsed-time diagnostics.
pub trait CycleCounterPort {
    fn init_dwt_cycle_counter(dcb: &mut DCB, dwt: &mut DWT) -> bool;
    fn cycle_count() -> u32;
    fn reset_elapse_counter();
    fn elapse_cycles() -> u32;
    fn elapse_msec() -> u32;
    fn elapse_msec_since(start_cycle: u32) -> u32;
}

/// Complete scheduler platform contract for a Cortex-M-style port.
pub trait Platform:
    ThreadStackPort + CriticalSectionPort + ContextSwitchPort + SchedulerTimerPort + CycleCounterPort
{
}

impl<T> Platform for T where
    T: ThreadStackPort
        + CriticalSectionPort
        + ContextSwitchPort
        + SchedulerTimerPort
        + CycleCounterPort
{
}

/// Hardware-backed Cortex-M platform.
pub struct CortexMPlatform;

/// Host fallback used by non-ARM tests and documentation builds.
pub struct HostPlatform;

#[cfg(target_arch = "arm")]
pub type DefaultPlatform = CortexMPlatform;

#[cfg(not(target_arch = "arm"))]
pub type DefaultPlatform = HostPlatform;

#[allow(dead_code)]
pub(crate) const SCHEDULER_TIMER_RELOAD_BITS: u32 =
    <DefaultPlatform as SchedulerTimerPort>::RELOAD_BITS;
pub(crate) const SCHEDULER_TIMER_RELOAD_MIN: u32 =
    <DefaultPlatform as SchedulerTimerPort>::RELOAD_MIN;
pub(crate) const SCHEDULER_TIMER_RELOAD_MAX: u32 =
    <DefaultPlatform as SchedulerTimerPort>::RELOAD_MAX;

pub(crate) const THREAD_STACK_ALIGNMENT: usize =
    <DefaultPlatform as ThreadStackPort>::STACK_ALIGNMENT;
pub(crate) const THREAD_INITIAL_FRAME_WORDS: usize =
    <DefaultPlatform as ThreadStackPort>::INITIAL_FRAME_WORDS;

impl ThreadStackPort for CortexMPlatform {
    const STACK_ALIGNMENT: usize = CORTEX_M_THREAD_STACK_ALIGNMENT;
    const INITIAL_FRAME_WORDS: usize = CORTEX_M_THREAD_INITIAL_FRAME_WORDS;

    unsafe fn init_thread_stack(
        sp: *mut u32,
        entry: ThreadEntry,
        arg: *mut c_void,
    ) -> InitialThreadContext {
        unsafe { init_cortex_m_thread_stack(sp, entry, arg) }
    }
}

impl ThreadStackPort for HostPlatform {
    const STACK_ALIGNMENT: usize = CORTEX_M_THREAD_STACK_ALIGNMENT;
    const INITIAL_FRAME_WORDS: usize = CORTEX_M_THREAD_INITIAL_FRAME_WORDS;

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
        sp = ((sp as usize) & !(CORTEX_M_THREAD_STACK_ALIGNMENT - 1)) as *mut u32;

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
impl CriticalSectionPort for CortexMPlatform {
    fn critical_section<R, F>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        cortex_m::interrupt::free(|_| f())
    }
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

#[cfg(all(not(target_arch = "arm"), not(test)))]
fn host_critical_section<R, F>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

#[cfg(not(target_arch = "arm"))]
impl CriticalSectionPort for HostPlatform {
    fn critical_section<R, F>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        host_critical_section(f)
    }
}

#[cfg(target_arch = "arm")]
impl ContextSwitchPort for CortexMPlatform {
    fn request_context_switch() {
        SCB::set_pendsv();
    }

    unsafe fn spawn_main_thread(thread: ThreadHandle) -> ! {
        unsafe { super::ctx_switch::spawn_main_thread(thread) }
    }
}

#[cfg(not(target_arch = "arm"))]
impl ContextSwitchPort for HostPlatform {
    fn request_context_switch() {}

    unsafe fn spawn_main_thread(_thread: ThreadHandle) -> ! {
        panic!("spawn_main_thread is only available on Cortex-M targets")
    }
}

/// Spawn the first scheduler thread using the platform context-switch entry.
///
/// # Safety
///
/// `thread` must refer to a live thread created by a thread builder for the
/// active platform. Its stack and thread storage must outlive all scheduler
/// use, `init_ktimer_queue` and `init_cfs` must have completed, and no other
/// scheduler thread may already be running.
///
/// Call this only from privileged single-core startup code after the platform
/// exception handlers and scheduler timer are configured enough for the
/// context-switch backend to restore the thread.
pub unsafe fn spawn_main_thread(thread: ThreadHandle) -> ! {
    unsafe { <DefaultPlatform as ContextSwitchPort>::spawn_main_thread(thread) }
}

#[cfg(target_arch = "arm")]
impl SchedulerTimerPort for CortexMPlatform {
    const RELOAD_BITS: u32 = CORTEX_M_SCHEDULER_TIMER_RELOAD_BITS;
    const RELOAD_MIN: u32 = CORTEX_M_SCHEDULER_TIMER_RELOAD_MIN;
    const RELOAD_MAX: u32 = CORTEX_M_SCHEDULER_TIMER_RELOAD_MAX;

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

impl SchedulerTimerPort for HostPlatform {
    const RELOAD_BITS: u32 = CORTEX_M_SCHEDULER_TIMER_RELOAD_BITS;
    const RELOAD_MIN: u32 = CORTEX_M_SCHEDULER_TIMER_RELOAD_MIN;
    const RELOAD_MAX: u32 = CORTEX_M_SCHEDULER_TIMER_RELOAD_MAX;

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

#[cfg(not(target_arch = "arm"))]
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

/// Build the initial thread stack frame consumed by SVCall/PendSV.
///
/// After the context switch restores r4-r11 and sets PSP, exception return
/// consumes the standard hardware frame: r0, r1, r2, r3, r12, lr, pc, xPSR.
///
/// # Safety
///
/// `sp` must be the exclusive top-of-stack pointer for writable stack storage
/// owned by the thread being initialized. At least `THREAD_INITIAL_FRAME_WORDS`
/// 32-bit words below `sp` must be valid to write, and `sp` must satisfy
/// `THREAD_STACK_ALIGNMENT`.
///
/// The stack storage must outlive all scheduler use of the thread. `entry`
/// must use the `ThreadEntry` ABI and must not return. `arg` is passed through
/// unchanged; the caller must ensure it remains valid for whatever the entry
/// function does with it.
pub(crate) unsafe fn init_thread_stack(
    sp: *mut u32,
    entry: ThreadEntry,
    arg: *mut c_void,
) -> InitialThreadContext {
    unsafe { <DefaultPlatform as ThreadStackPort>::init_thread_stack(sp, entry, arg) }
}

/// Run `f` while maskable interrupts are disabled.
pub(crate) fn critical_section<R>(f: impl FnOnce() -> R) -> R {
    <DefaultPlatform as CriticalSectionPort>::critical_section(f)
}

pub(crate) fn request_context_switch() {
    <DefaultPlatform as ContextSwitchPort>::request_context_switch();
}

/// Return the currently programmed scheduler-timer reload value.
pub(crate) fn scheduler_timer_reload() -> Option<u32> {
    <DefaultPlatform as SchedulerTimerPort>::reload()
}

/// Return the scheduler timer's current down-counter value.
pub(crate) fn scheduler_timer_current() -> Option<u32> {
    <DefaultPlatform as SchedulerTimerPort>::current()
}

/// Program the scheduler timer for the next interval and clear elapsed state.
pub(crate) fn program_scheduler_timer_reload(reload: u32) -> bool {
    <DefaultPlatform as SchedulerTimerPort>::program_reload(reload)
}

pub fn init_dwt_cycle_counter(dcb: &mut DCB, dwt: &mut DWT) -> bool {
    <DefaultPlatform as CycleCounterPort>::init_dwt_cycle_counter(dcb, dwt)
}

pub fn dwt_cycle_count() -> u32 {
    <DefaultPlatform as CycleCounterPort>::cycle_count()
}

/// Reset the baseline used by `get_elapse_cycles` and `get_elapse_msec`.
pub fn reset_elapse_counter() {
    <DefaultPlatform as CycleCounterPort>::reset_elapse_counter();
}

/// Return elapsed DWT cycles since the last DWT elapsed-counter reset.
///
/// The subtraction is wrapping, so the value is valid for intervals shorter
/// than one full 32-bit DWT counter wrap.
pub fn get_elapse_cycles() -> u32 {
    <DefaultPlatform as CycleCounterPort>::elapse_cycles()
}

/// Return elapsed milliseconds since the last DWT elapsed-counter reset.
///
/// `update_sys_clk_freq` must be called before using this conversion.
pub fn get_elapse_msec() -> u32 {
    <DefaultPlatform as CycleCounterPort>::elapse_msec()
}

pub fn get_elapse_msec_since(start_cycle: u32) -> u32 {
    <DefaultPlatform as CycleCounterPort>::elapse_msec_since(start_cycle)
}
