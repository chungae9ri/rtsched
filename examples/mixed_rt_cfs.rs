#![no_std]
#![no_main]

mod common;

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m_rt::{entry, exception};

const FAST_PERIOD_TICKS: u32 = 40 * common::TICKS_PER_MS;
const FAST_DEADLINE_TICKS: u32 = 15 * common::TICKS_PER_MS;
const FAST_BUDGET_TICKS: u32 = 4 * common::TICKS_PER_MS;
const SLOW_PERIOD_TICKS: u32 = 100 * common::TICKS_PER_MS;
const SLOW_DEADLINE_TICKS: u32 = 60 * common::TICKS_PER_MS;
const SLOW_BUDGET_TICKS: u32 = 10 * common::TICKS_PER_MS;

static mut IDLE_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut IDLE_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut BACKGROUND_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut BACKGROUND_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut FAST_RT_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut FAST_RT_THREAD: MaybeUninit<rtsched::RtThread> = MaybeUninit::uninit();
static mut FAST_RT_TIMER: rtsched::RtKTimer = rtsched::RtKTimer::new_with_timing(
    rtsched::RtTiming::new(FAST_PERIOD_TICKS, FAST_DEADLINE_TICKS, FAST_BUDGET_TICKS),
    core::ptr::null_mut(),
    "fast_rt",
);

static mut SLOW_RT_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut SLOW_RT_THREAD: MaybeUninit<rtsched::RtThread> = MaybeUninit::uninit();
static mut SLOW_RT_TIMER: rtsched::RtKTimer = rtsched::RtKTimer::new_with_timing(
    rtsched::RtTiming::new(SLOW_PERIOD_TICKS, SLOW_DEADLINE_TICKS, SLOW_BUDGET_TICKS),
    core::ptr::null_mut(),
    "slow_rt",
);

static FAST_JOBS: AtomicU32 = AtomicU32::new(0);
static SLOW_JOBS: AtomicU32 = AtomicU32::new(0);
static BACKGROUND_LOOPS: AtomicU32 = AtomicU32::new(0);

#[entry]
fn main() -> ! {
    unsafe {
        common::init_scheduler();

        let idle = rtsched::CfsThreadBuilder::new("cpu_idle", common::cpu_idle, 16).spawn(
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        );
        rtsched::CfsThreadBuilder::new("background", background_work, 4).spawn(
            core::ptr::addr_of_mut!(BACKGROUND_THREAD),
            core::ptr::addr_of_mut!(BACKGROUND_STACK),
        );
        rtsched::RtThreadBuilder::new(
            "fast_rt",
            fast_rt_job,
            core::ptr::addr_of_mut!(FAST_RT_TIMER),
        )
        .spawn(
            core::ptr::addr_of_mut!(FAST_RT_THREAD),
            core::ptr::addr_of_mut!(FAST_RT_STACK),
        );
        rtsched::RtThreadBuilder::new(
            "slow_rt",
            slow_rt_job,
            core::ptr::addr_of_mut!(SLOW_RT_TIMER),
        )
        .spawn(
            core::ptr::addr_of_mut!(SLOW_RT_THREAD),
            core::ptr::addr_of_mut!(SLOW_RT_STACK),
        );

        rtsched::register_idle_thread(idle);

        let Some(mut peripherals) = cortex_m::Peripherals::take() else {
            common::idle_forever();
        };
        common::configure_systick(&mut peripherals.SYST);

        rtsched::spawn_main_thread(idle)
    }
}

extern "C" fn background_work(_arg: *mut c_void) -> ! {
    loop {
        BACKGROUND_LOOPS.fetch_add(1, Ordering::Relaxed);
        rtsched::msleepyi(100);
    }
}

extern "C" fn fast_rt_job(_arg: *mut c_void) -> ! {
    loop {
        rtsched::set_rt_thread_start_time(0);
        FAST_JOBS.fetch_add(1, Ordering::Relaxed);
        common::spin(4_000);
        rtsched::yieldyi();
    }
}

extern "C" fn slow_rt_job(_arg: *mut c_void) -> ! {
    loop {
        rtsched::set_rt_thread_start_time(0);
        SLOW_JOBS.fetch_add(1, Ordering::Relaxed);
        common::spin(12_000);
        rtsched::yieldyi();
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
