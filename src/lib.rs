#![cfg_attr(not(test), no_std)]

//! Runtime scheduler primitives for `no_std` embedded applications.
//!
//! `rtsched` provides the core pieces needed to create, queue, block, wake, and
//! context-switch application threads on a single microcontroller core. It
//! combines CFS-style background scheduling with soft real-time scheduling built
//! on kernel timers ordered by deadline.
//!
//! Board crates own the hardware setup: clocks, timer interrupts,
//! architecture-specific context-switch entry points, stack storage, and
//! concrete thread storage. A typical board initializes the timer queue with
//! [`init_ktimer_queue`], configures CFS with [`init_cfs`], creates
//! [`CfsThread`] and [`RtThread`] values with their builders, registers an idle
//! CFS thread with [`register_idle_thread`], and starts execution with
//! [`spawn_main_thread`].
//!
//! The crate is `no_std` for embedded builds. Host-only test support provides
//! scheduler state serialization so the same core data structures can be
//! exercised by unit tests.
//!
//! # Scheduling Model
//!
//! - [`CfsThread`] uses a red-black-tree run queue ordered by virtual runtime.
//! - [`RtThread`] uses an [`RtKTimer`] entry for deadline-based scheduling.
//! - Waiting threads move through a wait queue and can be resumed by timer
//!   expiry or scheduler wake-up paths.
//! - [`handle_sched_tick`] advances timers and requests dispatch from the
//!   target-specific context-switch path.
//!
//! # Safety
//!
//! This crate exposes low-level scheduler entry points that operate on raw
//! pointers, intrusive queue links, global scheduler state, and
//! architecture-specific stack frames. Public `unsafe` functions document their
//! caller obligations individually. Board code must ensure thread and stack
//! storage outlives all scheduler use and that initialization happens before
//! scheduler interrupts can observe the global state.
//!
/// Crate version taken from Cargo metadata at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod arch;

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` while scheduler globals are protected from interrupt or test-thread
/// interleaving.
///
/// On Cortex-M this delegates to the architecture platform interface, which
/// masks interrupts in a nest-safe way. Host tests use a reentrant mutex-backed
/// critical section so nested scheduler calls do not deadlock and parallel
/// tests still serialize global state access.
pub(crate) fn critical_section<R>(f: impl FnOnce() -> R) -> R {
    crate::arch::platform::critical_section(f)
}

mod clock;
mod ktimer;
#[doc(hidden)]
pub mod print;
mod rbtree;
mod runq;
mod sched;
mod thread;
mod trace;
mod waitq;

/// Re-exports of core scheduler primitives for convenient use in application code.
pub use thread::{
    AlignedStack, CfsThread, CfsThreadBuilder, RtThread, RtThreadBuilder, SchedInfo, ThreadCtx,
    ThreadEntry, ThreadHandle, ThreadId, ThreadRef, ThreadSpawnError, ThreadStart, ThreadState,
    current_rt_thread_runtime, msleepyi, set_rt_thread_start_time, yieldyi,
};

pub use clock::{sys_clk_freq, ticks_per_ms, update_sys_clk_freq};

pub use print::set_print_fn;

pub use arch::cm::platform::{
    ContextSwitchPort, CortexMPlatform, CriticalSectionPort, CycleCounterPort, DefaultPlatform,
    HostPlatform, InitialThreadContext, Platform, SchedulerTimerPort, ThreadStackPort,
    dwt_cycle_count, get_elapse_cycles, get_elapse_msec, get_elapse_msec_since,
    init_dwt_cycle_counter, reset_elapse_counter, spawn_main_thread,
};

pub use ktimer::{
    RtKTimer, RtTiming, dequeue_rt_thread_to_waitq, enqueue_rt_thread_from_waitq,
    init_ktimer_queue, is_active_ktimer, next_ktimer_reload, traverse_ktimer_queue,
    traverse_ktimer_queue_fn,
};

pub use runq::{dequeue_cfs_thread_to_waitq, traverse_run_queue_fn};

pub use sched::{handle_sched_tick, init_cfs, register_idle_thread, traverse_idle_thread_fn};

pub use trace::{
    TraceCounters, TraceEvent, TraceFn, TraceThread, clear_trace_fn, reset_trace_counters,
    set_trace_fn, trace_counters,
};

pub use waitq::{WaitQueueError, traverse_wait_queue_fn};

#[cfg(all(test, not(target_arch = "arm")))]
mod tests {
    use super::critical_section;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn host_critical_section_allows_nested_calls() {
        let value = critical_section(|| critical_section(|| 42));

        assert_eq!(value, 42);
    }

    #[test]
    fn host_critical_section_serializes_parallel_threads() {
        let in_section = Arc::new(AtomicBool::new(false));
        let overlap_count = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..8 {
            let in_section = Arc::clone(&in_section);
            let overlap_count = Arc::clone(&overlap_count);

            threads.push(thread::spawn(move || {
                for _ in 0..128 {
                    critical_section(|| {
                        if in_section.swap(true, Ordering::SeqCst) {
                            overlap_count.fetch_add(1, Ordering::SeqCst);
                        }

                        thread::yield_now();
                        in_section.store(false, Ordering::SeqCst);
                    });
                }
            }));
        }

        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(overlap_count.load(Ordering::SeqCst), 0);
    }
}
