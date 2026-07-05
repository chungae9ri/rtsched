#![cfg_attr(not(test), no_std)]

/// Crate version taken from Cargo metadata at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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

/// Run `f` while scheduler globals are protected from interrupt or test-thread
/// interleaving.
///
/// On Cortex-M this delegates to `cortex_m::interrupt::free`, which is nest-safe
/// because it restores interrupts only when they were active before entry. Host
/// tests use a reentrant mutex-backed critical section so nested scheduler calls
/// do not deadlock and parallel tests still serialize global state access.
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
mod arch;
#[cfg(not(target_arch = "arm"))]
mod arch {
    pub mod ctx_swtich {
        use crate::thread::ThreadCtx;

        pub unsafe fn spawn_main_thread(_thread: *mut ThreadCtx) -> ! {
            panic!("spawn_main_thread is only available on Cortex-M targets")
        }

        pub(crate) fn request_context_switch() {}
    }

    pub mod timer_cm {
        use cortex_m::peripheral::{DCB, DWT};

        pub fn init_dwt_cycle_counter(_dcb: &mut DCB, _dwt: &mut DWT) -> bool {
            false
        }

        pub fn dwt_cycle_count() -> u32 {
            0
        }

        pub fn reset_elapse_counter() {}

        pub fn get_elapse_cycles() -> u32 {
            0
        }

        pub fn get_elapse_msec() -> u32 {
            0
        }

        pub fn get_elapse_msec_since(_start_cycle: u32) -> u32 {
            0
        }
    }
}
mod clock;
mod ktimer;
#[doc(hidden)]
pub mod print;
mod rbtree;
mod runq;
mod sched;
mod thread;
mod waitq;

/// Re-exports of core scheduler primitives for convenient use in application code.
pub use thread::{
    AlignedStack, CfsThread, CfsThreadBuilder, RtThread, RtThreadBuilder, SchedInfo, ThreadCtx,
    ThreadEntry, ThreadStart, ThreadState, current_rt_thread_runtime, msleepyi,
    set_rt_thread_start_time, yieldyi,
};

pub use clock::{sys_clk_freq, ticks_per_ms, update_sys_clk_freq};

pub use print::set_print_fn;

pub use arch::ctx_swtich::spawn_main_thread;

pub use arch::timer_cm::{
    dwt_cycle_count, get_elapse_cycles, get_elapse_msec, get_elapse_msec_since,
    init_dwt_cycle_counter, reset_elapse_counter,
};

pub use ktimer::{
    RtKTimer, dequeue_rt_thread_to_waitq, enqueue_rt_thread_from_waitq, init_ktimer_queue,
    is_active_ktimer, next_ktimer_reload, traverse_ktimer_queue, traverse_ktimer_queue_fn,
};

pub use runq::{dequeue_cfs_thread_to_waitq, traverse_run_queue_fn};

pub use sched::{handle_sched_tick, init_cfs};

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
