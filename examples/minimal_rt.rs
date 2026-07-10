#![no_std]
#![no_main]

mod common;

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m_rt::{entry, exception};

const RT_PERIOD_TICKS: u32 = 50 * common::TICKS_PER_MS;
const RT_DEADLINE_TICKS: u32 = 20 * common::TICKS_PER_MS;
const RT_BUDGET_TICKS: u32 = 5 * common::TICKS_PER_MS;

static mut IDLE_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut IDLE_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();

static mut CONTROL_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut CONTROL_THREAD: MaybeUninit<rtsched::RtThread> = MaybeUninit::uninit();
static mut CONTROL_TIMER: rtsched::RtKTimer = rtsched::RtKTimer::new_with_timing(
    rtsched::RtTiming::new(RT_PERIOD_TICKS, RT_DEADLINE_TICKS, RT_BUDGET_TICKS),
    core::ptr::null_mut(),
    "control",
);

static CONTROL_JOBS: AtomicU32 = AtomicU32::new(0);

#[entry]
fn main() -> ! {
    unsafe {
        common::init_scheduler();

        let idle = rtsched::CfsThreadBuilder::new("cpu_idle", common::cpu_idle, 16).spawn(
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        );
        rtsched::RtThreadBuilder::new(
            "control",
            control_loop,
            core::ptr::addr_of_mut!(CONTROL_TIMER),
        )
        .spawn(
            core::ptr::addr_of_mut!(CONTROL_THREAD),
            core::ptr::addr_of_mut!(CONTROL_STACK),
        );

        rtsched::register_idle_thread(idle);

        let Some(mut peripherals) = cortex_m::Peripherals::take() else {
            common::idle_forever();
        };
        common::configure_systick(&mut peripherals.SYST);

        rtsched::spawn_main_thread(idle)
    }
}

extern "C" fn control_loop(_arg: *mut c_void) -> ! {
    loop {
        rtsched::set_rt_thread_start_time(0);
        CONTROL_JOBS.fetch_add(1, Ordering::Relaxed);
        common::spin(8_000);
        rtsched::yieldyi();
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
