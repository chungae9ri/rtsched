#![no_std]
#![no_main]

mod common;

use core::ffi::c_void;
use core::mem::MaybeUninit;
//use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m_rt::{entry, exception};

static mut IDLE_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut IDLE_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut CONTENDER_A_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut CONTENDER_A_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut CONTENDER_B_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut CONTENDER_B_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut RT_THREAD1_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut RT_THREAD1: MaybeUninit<rtsched::RtThread> = MaybeUninit::uninit();
const RT_THREAD1_PERIOD_MS: u32 = 100;
const RT_THREAD1_PERIOD_TICKS: u32 = RT_THREAD1_PERIOD_MS * { common::TICKS_PER_MS };
static mut RT_THREAD1_TIMER_ENTITY: rtsched::RtKTimer =
    rtsched::RtKTimer::new(RT_THREAD1_PERIOD_TICKS, core::ptr::null_mut(), "rt_thread1");

static SIGNAL: rtsched::BinarySemaphore = rtsched::BinarySemaphore::available();

#[entry]
fn main() -> ! {
    unsafe {
        common::init_scheduler();

        let idle = rtsched::CfsThreadBuilder::new("cpu_idle", common::cpu_idle, 16).spawn(
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        );
        rtsched::CfsThreadBuilder::new("contender_a", contender_a, 2).spawn(
            core::ptr::addr_of_mut!(CONTENDER_A_THREAD),
            core::ptr::addr_of_mut!(CONTENDER_A_STACK),
        );
        rtsched::CfsThreadBuilder::new("contender_b", contender_b, 1).spawn(
            core::ptr::addr_of_mut!(CONTENDER_B_THREAD),
            core::ptr::addr_of_mut!(CONTENDER_B_STACK),
        );
        rtsched::RtThreadBuilder::new(
            "rt_thread1",
            rt_thread1_runner,
            core::ptr::addr_of_mut!(RT_THREAD1_TIMER_ENTITY),
        )
        .spawn(
            core::ptr::addr_of_mut!(RT_THREAD1),
            core::ptr::addr_of_mut!(RT_THREAD1_STACK),
        );

        rtsched::register_idle_thread(idle);

        let Some(mut peripherals) = cortex_m::Peripherals::take() else {
            common::idle_forever();
        };
        common::configure_systick(&mut peripherals.SYST);

        rtsched::spawn_main_thread(idle)
    }
}

extern "C" fn contender_a(_arg: *mut c_void) -> ! {
    loop {
        rtsched::msleepyi(75);

        if SIGNAL.take().is_ok() {
            common::spin(10000);
            SIGNAL.give().unwrap();
        }
    }
}

extern "C" fn contender_b(_arg: *mut c_void) -> ! {
    loop {
        rtsched::msleepyi(100);

        if SIGNAL.take().is_ok() {
            common::spin(10000);
            SIGNAL.give().unwrap();
        }
    }
}

extern "C" fn rt_thread1_runner(_arg: *mut c_void) -> ! {
    loop {
        rtsched::set_rt_thread_start_time(0);

        if SIGNAL.take().is_err() {
            rtsched::yieldyi();
            continue;
        }

        for _ in 0..5 {
            common::spin(10_000);
        }

        SIGNAL.give().unwrap();

        rtsched::yieldyi();
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
