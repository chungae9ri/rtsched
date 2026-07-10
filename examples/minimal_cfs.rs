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

static mut WORKER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut WORKER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static WORKER_RUNS: AtomicU32 = AtomicU32::new(0);

#[entry]
fn main() -> ! {
    unsafe {
        common::init_scheduler();

        let idle = rtsched::CfsThreadBuilder::new("cpu_idle", common::cpu_idle, 16).spawn(
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        );
        rtsched::CfsThreadBuilder::new("worker", cfs_worker, 1).spawn(
            core::ptr::addr_of_mut!(WORKER_THREAD),
            core::ptr::addr_of_mut!(WORKER_STACK),
        );

        rtsched::register_idle_thread(idle);

        let Some(mut peripherals) = cortex_m::Peripherals::take() else {
            common::idle_forever();
        };
        common::configure_systick(&mut peripherals.SYST);

        rtsched::spawn_main_thread(idle)
    }
}

extern "C" fn cfs_worker(_arg: *mut c_void) -> ! {
    loop {
        WORKER_RUNS.fetch_add(1, Ordering::Relaxed);
        common::spin(2_000);
        rtsched::yieldyi();
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
