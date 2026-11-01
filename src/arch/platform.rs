// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Shared platform traits and dispatch helpers for the scheduler core.
//!
//! Platform implementations live in `arch/cm` and `arch/host`. Scheduler code
//! uses this module to call the implementation selected by `DefaultPlatform`.

use core::ffi::c_void;
use core::mem::MaybeUninit;

use cortex_m::peripheral::{DCB, DWT};

use crate::thread::{
    AlignedStack, IdleThread, ThreadEntry, ThreadHandle, ThreadStart, spawn_idle_thread,
};

#[cfg(all(target_arch = "arm", not(feature = "host")))]
pub use super::cm::platform::CortexMPlatform;
#[cfg(feature = "host")]
pub use super::host::platform::HostPlatform;

#[cfg(all(target_arch = "arm", not(feature = "host")))]
pub(crate) use super::cm::platform::blocking_context_available;
#[cfg(feature = "host")]
pub(crate) use super::host::platform::blocking_context_available;

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
    /// `thread` must refer to the live registered idle thread created for the
    /// active platform. Its stack and thread storage must outlive all scheduler
    /// use, the scheduler queues must already be initialized, and no other
    /// scheduler thread may already be running.
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

/// Complete scheduler platform contract.
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

#[cfg(all(target_arch = "arm", not(feature = "host")))]
pub type DefaultPlatform = CortexMPlatform;

#[cfg(feature = "host")]
pub type DefaultPlatform = HostPlatform;

pub(crate) const SCHEDULER_TIMER_RELOAD_MIN: u32 =
    <DefaultPlatform as SchedulerTimerPort>::RELOAD_MIN;
pub(crate) const SCHEDULER_TIMER_RELOAD_MAX: u32 =
    <DefaultPlatform as SchedulerTimerPort>::RELOAD_MAX;

pub(crate) const THREAD_STACK_ALIGNMENT: usize =
    <DefaultPlatform as ThreadStackPort>::STACK_ALIGNMENT;
pub(crate) const THREAD_INITIAL_FRAME_WORDS: usize =
    <DefaultPlatform as ThreadStackPort>::INITIAL_FRAME_WORDS;

/// Initialize the idle thread and spawn it as the first scheduler thread.
///
/// # Safety
///
/// `thread` must be non-null, properly aligned, uniquely owned writable storage
/// for one `IdleThread`. `stack` must be non-null, uniquely owned writable stack
/// storage for the idle thread, with a top address aligned for the active
/// platform and enough room for the initial platform frame.
///
/// Both storage objects must remain at fixed addresses and outlive all
/// scheduler use. `init_ktimer_queue` must have completed, and if CFS scheduling
/// is used, `init_cfs` must have completed. The idle thread is registered as the
/// scheduler fallback before the platform context-switch entry restores it.
///
/// Call this only from privileged single-core startup code after the platform
/// exception handlers and scheduler timer are configured enough for the
/// context-switch backend to restore the thread, and before any other scheduler
/// thread is running.
pub unsafe fn spawn_main_thread<const N: usize>(
    name: &'static str,
    entry: ThreadEntry,
    thread: *mut MaybeUninit<IdleThread>,
    stack: *mut AlignedStack<N>,
) -> ! {
    unsafe {
        let thread = spawn_idle_thread(ThreadStart::new(name, entry), thread, stack);
        crate::sched::register_idle_thread(thread);
        <DefaultPlatform as ContextSwitchPort>::spawn_main_thread(thread)
    }
}

/// Build the initial thread stack frame for the selected platform.
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
