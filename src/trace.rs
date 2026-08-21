// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Lightweight scheduler tracing hooks.

#[cfg(feature = "sched-isr-timing")]
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::thread::ThreadCtx;

/// Snapshot of a scheduler-visible thread for trace events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceThread {
    pub id: u32,
    pub name: &'static str,
    pub is_cfs: bool,
}

impl TraceThread {
    fn from_ptr(thread: *const ThreadCtx) -> Option<Self> {
        if thread.is_null() {
            return None;
        }

        let thread = unsafe { &*thread };
        Some(Self {
            id: thread.id,
            name: thread.name,
            is_cfs: thread.is_cfs,
        })
    }
}

/// Scheduler events emitted by the tracing hook.
///
/// Callbacks run from scheduler paths, often inside a critical section or an
/// interrupt-triggered context switch path, so they should stay short and
/// non-blocking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceEvent {
    ContextSwitch {
        from: Option<TraceThread>,
        to: TraceThread,
    },
    DeadlineMiss {
        thread: TraceThread,
        runtime_ticks: u32,
        relative_deadline_ticks: u32,
    },
    Wakeup {
        thread: TraceThread,
    },
    Yield {
        thread: TraceThread,
        elapsed_ticks: u32,
    },
}

/// Saturating counters for common scheduler events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraceCounters {
    pub context_switches: u32,
    pub deadline_misses: u32,
    pub wakeups: u32,
    pub yields: u32,
}

/// DWT-cycle timing for the SysTick-to-PendSV path.
///
/// On boards where SysTick uses the core clock, these cycle counts are the same
/// raw tick unit used by scheduler deadlines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedIsrTiming {
    pub samples: u32,
    pub last_ticks: u32,
    pub max_ticks: u32,
}

pub type TraceFn = fn(TraceEvent);

static TRACE_FN: AtomicUsize = AtomicUsize::new(0);
static CONTEXT_SWITCHES: AtomicU32 = AtomicU32::new(0);
static DEADLINE_MISSES: AtomicU32 = AtomicU32::new(0);
static WAKEUPS: AtomicU32 = AtomicU32::new(0);
static YIELDS: AtomicU32 = AtomicU32::new(0);

#[cfg(feature = "sched-isr-timing")]
#[unsafe(no_mangle)]
pub static mut SCHED_TICK_TO_PENDSV_ENABLED: u32 = 0;
#[cfg(feature = "sched-isr-timing")]
#[unsafe(no_mangle)]
pub static mut SCHED_TICK_TO_PENDSV_ARMED: u32 = 0;
#[cfg(feature = "sched-isr-timing")]
#[unsafe(no_mangle)]
pub static mut SCHED_TICK_TO_PENDSV_START_CYCLE: u32 = 0;
#[cfg(feature = "sched-isr-timing")]
#[unsafe(no_mangle)]
pub static mut SCHED_TICK_TO_PENDSV_LAST_TICKS: u32 = 0;
#[cfg(feature = "sched-isr-timing")]
#[unsafe(no_mangle)]
pub static mut SCHED_TICK_TO_PENDSV_MAX_TICKS: u32 = 0;
#[cfg(feature = "sched-isr-timing")]
#[unsafe(no_mangle)]
pub static mut SCHED_TICK_TO_PENDSV_SAMPLES: u32 = 0;

/// Register a trace callback for scheduler events.
pub fn set_trace_fn(trace_fn: TraceFn) {
    TRACE_FN.store(trace_fn as usize, Ordering::Release);
}

/// Remove any registered trace callback.
pub fn clear_trace_fn() {
    TRACE_FN.store(0, Ordering::Release);
}

/// Return the current scheduler trace counters.
pub fn trace_counters() -> TraceCounters {
    TraceCounters {
        context_switches: CONTEXT_SWITCHES.load(Ordering::Relaxed),
        deadline_misses: DEADLINE_MISSES.load(Ordering::Relaxed),
        wakeups: WAKEUPS.load(Ordering::Relaxed),
        yields: YIELDS.load(Ordering::Relaxed),
    }
}

/// Reset scheduler trace counters to zero.
pub fn reset_trace_counters() {
    CONTEXT_SWITCHES.store(0, Ordering::Relaxed);
    DEADLINE_MISSES.store(0, Ordering::Relaxed);
    WAKEUPS.store(0, Ordering::Relaxed);
    YIELDS.store(0, Ordering::Relaxed);
}

/// Mark the beginning of a SysTick interrupt for SysTick-to-PendSV timing.
///
/// Call this as the first statement in the board's `SysTick` handler after the
/// DWT cycle counter has been initialized. A sample is recorded only when that
/// same `handle_sched_tick` path requests a context switch.
#[inline]
pub fn mark_sched_tick_to_pendsv_start() {
    #[cfg(feature = "sched-isr-timing")]
    {
        let start_cycle = crate::arch::platform::dwt_cycle_count();

        unsafe {
            ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_ENABLED, 1);
            ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_START_CYCLE, start_cycle);
        }
    }
}

#[inline]
pub(crate) fn arm_sched_tick_to_pendsv_sample() {
    #[cfg(feature = "sched-isr-timing")]
    {
        unsafe {
            if ptr::read_volatile(&raw const SCHED_TICK_TO_PENDSV_ENABLED) != 0 {
                ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_ARMED, 1);
            }
        }
    }
}

#[inline]
pub fn sched_tick_to_pendsv_timing() -> SchedIsrTiming {
    #[cfg(feature = "sched-isr-timing")]
    {
        unsafe {
            return SchedIsrTiming {
                samples: ptr::read_volatile(&raw const SCHED_TICK_TO_PENDSV_SAMPLES),
                last_ticks: ptr::read_volatile(&raw const SCHED_TICK_TO_PENDSV_LAST_TICKS),
                max_ticks: ptr::read_volatile(&raw const SCHED_TICK_TO_PENDSV_MAX_TICKS),
            };
        }
    }

    #[cfg(not(feature = "sched-isr-timing"))]
    {
        SchedIsrTiming::default()
    }
}

#[inline]
pub fn reset_sched_tick_to_pendsv_timing() {
    #[cfg(feature = "sched-isr-timing")]
    {
        unsafe {
            ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_ARMED, 0);
            ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_START_CYCLE, 0);
            ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_LAST_TICKS, 0);
            ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_MAX_TICKS, 0);
            ptr::write_volatile(&raw mut SCHED_TICK_TO_PENDSV_SAMPLES, 0);
        }
    }
}

pub(crate) fn record_context_switch(from: *const ThreadCtx, to: *const ThreadCtx) {
    if from == to {
        return;
    }

    let Some(to) = TraceThread::from_ptr(to) else {
        return;
    };

    increment(&CONTEXT_SWITCHES);
    emit(TraceEvent::ContextSwitch {
        from: TraceThread::from_ptr(from),
        to,
    });
}

pub(crate) fn record_deadline_miss(
    thread: *const ThreadCtx,
    runtime_ticks: u32,
    relative_deadline_ticks: u32,
) {
    let Some(thread) = TraceThread::from_ptr(thread) else {
        return;
    };

    increment(&DEADLINE_MISSES);
    emit(TraceEvent::DeadlineMiss {
        thread,
        runtime_ticks,
        relative_deadline_ticks,
    });
}

pub(crate) fn record_wakeup(thread: *const ThreadCtx) {
    let Some(thread) = TraceThread::from_ptr(thread) else {
        return;
    };

    increment(&WAKEUPS);
    emit(TraceEvent::Wakeup { thread });
}

pub(crate) fn record_yield(thread: *const ThreadCtx, elapsed_ticks: u32) {
    let Some(thread) = TraceThread::from_ptr(thread) else {
        return;
    };

    increment(&YIELDS);
    emit(TraceEvent::Yield {
        thread,
        elapsed_ticks,
    });
}

fn increment(counter: &AtomicU32) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
    });
}

fn emit(event: TraceEvent) {
    let trace_fn = TRACE_FN.load(Ordering::Acquire);
    if trace_fn != 0 {
        let trace_fn: TraceFn = unsafe { core::mem::transmute(trace_fn) };
        trace_fn(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::thread::ThreadState;
    use std::sync::Mutex;
    use std::vec::Vec;

    static EVENTS: Mutex<Vec<TraceEvent>> = Mutex::new(Vec::new());

    fn capture_event(event: TraceEvent) {
        EVENTS.lock().unwrap().push(event);
    }

    fn thread(id: u32, name: &'static str, is_cfs: bool) -> ThreadCtx {
        ThreadCtx {
            sp: 0,
            exc_return: 0,
            id,
            name,
            state: ThreadState::Ready,
            is_cfs,
        }
    }

    #[test]
    fn trace_records_counters_and_callback_events() {
        let _guard = TEST_LOCK.lock().unwrap();
        let from = thread(1, "from", true);
        let to = thread(2, "to", false);

        EVENTS.lock().unwrap().clear();
        reset_trace_counters();
        set_trace_fn(capture_event);

        record_context_switch(&from, &to);
        record_deadline_miss(&to, 37, 40);
        record_wakeup(&from);
        record_yield(&to, 5);

        clear_trace_fn();

        let counters = trace_counters();
        assert!(counters.context_switches >= 1);
        assert!(counters.deadline_misses >= 1);
        assert!(counters.wakeups >= 1);
        assert!(counters.yields >= 1);

        let from = TraceThread {
            id: 1,
            name: "from",
            is_cfs: true,
        };
        let to = TraceThread {
            id: 2,
            name: "to",
            is_cfs: false,
        };
        let events = EVENTS.lock().unwrap();
        assert!(events.contains(&TraceEvent::ContextSwitch {
            from: Some(from),
            to
        }));
        assert!(events.contains(&TraceEvent::DeadlineMiss {
            thread: to,
            runtime_ticks: 37,
            relative_deadline_ticks: 40,
        }));
        assert!(events.contains(&TraceEvent::Wakeup { thread: from }));
        assert!(events.contains(&TraceEvent::Yield {
            thread: to,
            elapsed_ticks: 5,
        }));
    }
}
