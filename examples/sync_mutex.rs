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

static mut OWNER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut OWNER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut CONTENDER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut CONTENDER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static SHARED_COUNTER: rtsched::Mutex<u32> = rtsched::Mutex::new(0);
static OWNER_UPDATES: AtomicU32 = AtomicU32::new(0);
static CONTENDER_UPDATES: AtomicU32 = AtomicU32::new(0);
static LOCK_ERRORS: AtomicU32 = AtomicU32::new(0);

#[entry]
fn main() -> ! {
    unsafe {
        common::init_scheduler();

        let idle = rtsched::CfsThreadBuilder::new("cpu_idle", common::cpu_idle, 16).spawn(
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        );
        rtsched::CfsThreadBuilder::new("mutex_owner", mutex_owner, 1).spawn(
            core::ptr::addr_of_mut!(OWNER_THREAD),
            core::ptr::addr_of_mut!(OWNER_STACK),
        );
        rtsched::CfsThreadBuilder::new("mutex_contender", mutex_contender, 1).spawn(
            core::ptr::addr_of_mut!(CONTENDER_THREAD),
            core::ptr::addr_of_mut!(CONTENDER_STACK),
        );

        rtsched::register_idle_thread(idle);

        let Some(mut peripherals) = cortex_m::Peripherals::take() else {
            common::idle_forever();
        };
        common::configure_systick(&mut peripherals.SYST);

        rtsched::spawn_main_thread(idle)
    }
}

extern "C" fn mutex_owner(_arg: *mut c_void) -> ! {
    loop {
        match SHARED_COUNTER.lock() {
            Ok(mut counter) => {
                *counter += 1;
                OWNER_UPDATES.fetch_add(1, Ordering::Relaxed);
                drop(counter);
                rtsched::msleepyi(25);
            }
            Err(_) => {
                LOCK_ERRORS.fetch_add(1, Ordering::Relaxed);
                rtsched::yieldyi();
            }
        }

        rtsched::yieldyi();
    }
}

extern "C" fn mutex_contender(_arg: *mut c_void) -> ! {
    loop {
        match SHARED_COUNTER.lock() {
            Ok(mut counter) => {
                *counter += 1;
                CONTENDER_UPDATES.fetch_add(1, Ordering::Relaxed);
                drop(counter);
                common::spin(2_000);
            }
            Err(_) => {
                LOCK_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
        }

        rtsched::yieldyi();
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
