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

static mut FAST_SLEEPER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut FAST_SLEEPER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut SLOW_SLEEPER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut SLOW_SLEEPER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static FAST_WAKEUPS: AtomicU32 = AtomicU32::new(0);
static SLOW_WAKEUPS: AtomicU32 = AtomicU32::new(0);

#[entry]
fn main() -> ! {
    unsafe {
        common::init_scheduler();

        let idle = rtsched::CfsThreadBuilder::new("cpu_idle", common::cpu_idle, 16).spawn(
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        );
        rtsched::CfsThreadBuilder::new("fast_sleeper", fast_sleeper, 1).spawn(
            core::ptr::addr_of_mut!(FAST_SLEEPER_THREAD),
            core::ptr::addr_of_mut!(FAST_SLEEPER_STACK),
        );
        rtsched::CfsThreadBuilder::new("slow_sleeper", slow_sleeper, 1).spawn(
            core::ptr::addr_of_mut!(SLOW_SLEEPER_THREAD),
            core::ptr::addr_of_mut!(SLOW_SLEEPER_STACK),
        );

        rtsched::register_idle_thread(idle);

        let Some(mut peripherals) = cortex_m::Peripherals::take() else {
            common::idle_forever();
        };
        common::configure_systick(&mut peripherals.SYST);

        rtsched::spawn_main_thread(idle)
    }
}

extern "C" fn fast_sleeper(_arg: *mut c_void) -> ! {
    loop {
        FAST_WAKEUPS.fetch_add(1, Ordering::Relaxed);
        rtsched::msleepyi(50);
    }
}

extern "C" fn slow_sleeper(_arg: *mut c_void) -> ! {
    loop {
        SLOW_WAKEUPS.fetch_add(1, Ordering::Relaxed);
        rtsched::msleepyi(250);
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
