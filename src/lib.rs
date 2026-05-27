#![no_std]

/// Crate version taken from Cargo metadata at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod arch;
mod clock;
pub mod ktimer;
mod rbtree;
mod runq;
mod sched;
mod thread;
mod waitq;

/// Re-exports of core scheduler primitives for convenient use in application code.
pub use thread::{
    AlignedStack, CfsThread, RtThread, ThreadCtx, ThreadState, forkyi, msleepyi,
    set_rt_thread_start_time, yieldyi,
};

pub use clock::{sys_clk_freq, ticks_per_ms, update_sys_clk_freq};

pub use arch::timer_cm::{
    dwt_cycle_count, get_elapse_cycles, get_elapse_msec, get_elapse_msec_since,
    init_dwt_cycle_counter, reset_elapse_counter,
};

pub use ktimer::{
    KTimerEntity, RtKTimer, WaitKTimer, dequeue_ktimerq_to_waitq, enqueue_ktimer,
    enqueue_ktimerq_from_waitq, init_ktimer_queue, next_ktimer_reload, traverse_ktimer_queue,
    traverse_ktimer_queue_fn,
};

pub use runq::{dequeue_runq_to_waitq, enqueue_runq_from_waitq, traverse_run_queue};

pub use sched::{handle_systick, init_cfs, spawn_main_thread};

pub use waitq::WaitQueueError;
