#![cfg_attr(not(test), no_std)]

/// Crate version taken from Cargo metadata at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
pub mod ktimer;
#[doc(hidden)]
pub mod print;
mod rbtree;
mod runq;
mod sched;
mod thread;
mod waitq;

/// Re-exports of core scheduler primitives for convenient use in application code.
pub use thread::{
    AlignedStack, CfsThread, RtThread, ThreadCtx, ThreadState, current_rt_thread_runtime, forkyi,
    msleepyi, set_rt_thread_start_time, yieldyi,
};

pub use clock::{sys_clk_freq, ticks_per_ms, update_sys_clk_freq};

pub use print::set_print_fn;

pub use arch::ctx_swtich::spawn_main_thread;

pub use arch::timer_cm::{
    dwt_cycle_count, get_elapse_cycles, get_elapse_msec, get_elapse_msec_since,
    init_dwt_cycle_counter, reset_elapse_counter,
};

pub use ktimer::{
    KTimerEntity, RtKTimer, WaitKTimer, dequeue_ktimerq_to_waitq, enqueue_ktimer,
    enqueue_ktimerq_from_waitq, init_ktimer_queue, is_active_ktimer, next_ktimer_reload,
    traverse_ktimer_queue, traverse_ktimer_queue_fn,
};

pub use runq::{dequeue_runq_to_waitq, traverse_run_queue};

pub use sched::{handle_sched_tick, init_cfs};

pub use waitq::{WaitQueueError, traverse_wait_queue};
