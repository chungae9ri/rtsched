#![no_std]
#![no_main]

mod common;

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m_rt::{entry, exception};

static mut IDLE_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut IDLE_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut PRODUCER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut PRODUCER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut FAST_CONSUMER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut FAST_CONSUMER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut SLOW_CONSUMER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut SLOW_CONSUMER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static TOKENS: rtsched::CountingSemaphore = rtsched::CountingSemaphore::empty(3);
static PRODUCED_TOKENS: AtomicU32 = AtomicU32::new(0);
static FAST_TOKENS: AtomicU32 = AtomicU32::new(0);
static SLOW_TOKENS: AtomicU32 = AtomicU32::new(0);
static TOKEN_OVERFLOWS: AtomicU32 = AtomicU32::new(0);
static TAKE_ERRORS: AtomicU32 = AtomicU32::new(0);

#[entry]
fn main() -> ! {
    unsafe {
        common::init_scheduler();

        let idle = rtsched::CfsThreadBuilder::new("cpu_idle", common::cpu_idle, 16).spawn(
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        );
        rtsched::CfsThreadBuilder::new("token_producer", token_producer, 2).spawn(
            core::ptr::addr_of_mut!(PRODUCER_THREAD),
            core::ptr::addr_of_mut!(PRODUCER_STACK),
        );
        rtsched::CfsThreadBuilder::new("fast_consumer", fast_consumer, 1).spawn(
            core::ptr::addr_of_mut!(FAST_CONSUMER_THREAD),
            core::ptr::addr_of_mut!(FAST_CONSUMER_STACK),
        );
        rtsched::CfsThreadBuilder::new("slow_consumer", slow_consumer, 1).spawn(
            core::ptr::addr_of_mut!(SLOW_CONSUMER_THREAD),
            core::ptr::addr_of_mut!(SLOW_CONSUMER_STACK),
        );

        rtsched::register_idle_thread(idle);

        let Some(mut peripherals) = cortex_m::Peripherals::take() else {
            common::idle_forever();
        };
        common::configure_systick(&mut peripherals.SYST);

        rtsched::spawn_main_thread(idle)
    }
}

extern "C" fn token_producer(_arg: *mut c_void) -> ! {
    loop {
        for _ in 0..3 {
            if TOKENS.give().is_ok() {
                PRODUCED_TOKENS.fetch_add(1, Ordering::Relaxed);
            } else {
                TOKEN_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
            }
        }

        rtsched::msleepyi(120);
    }
}

extern "C" fn fast_consumer(_arg: *mut c_void) -> ! {
    loop {
        if TOKENS.take().is_ok() {
            FAST_TOKENS.fetch_add(1, Ordering::Relaxed);
            common::spin(1_000);
            rtsched::yieldyi();
        } else {
            TAKE_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

extern "C" fn slow_consumer(_arg: *mut c_void) -> ! {
    loop {
        if TOKENS.take().is_ok() {
            SLOW_TOKENS.fetch_add(1, Ordering::Relaxed);
            rtsched::msleepyi(40);
        } else {
            TAKE_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
