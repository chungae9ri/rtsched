use core::ffi::c_void;
use core::panic::PanicInfo;

use cortex_m::peripheral::{SYST, syst::SystClkSource};

pub const STACK_WORDS: usize = 512;
pub const SYS_CLK_HZ: u32 = 12_000_000;
pub const TICKS_PER_MS: u32 = SYS_CLK_HZ / 1000;
pub const CFS_PERIOD_TICKS: u32 = 30 * TICKS_PER_MS;
pub const CFS_EXEC_TICKS: u32 = 10 * TICKS_PER_MS;

pub fn init_scheduler() {
    unsafe {
        rtsched::update_sys_clk_freq(SYS_CLK_HZ);
        rtsched::init_ktimer_queue();
        rtsched::init_cfs(CFS_PERIOD_TICKS, CFS_EXEC_TICKS);
    }
}

pub fn configure_systick(syst: &mut SYST) {
    let Some(reload) = rtsched::next_ktimer_reload() else {
        idle_forever();
    };

    syst.set_clock_source(SystClkSource::Core);
    syst.set_reload(reload);
    syst.clear_current();
    syst.enable_interrupt();
    syst.enable_counter();
}

pub extern "C" fn cpu_idle(_arg: *mut c_void) -> ! {
    idle_forever();
}

pub fn idle_forever() -> ! {
    loop {
        cortex_m::asm::wfi();
    }
}

#[allow(dead_code)]
pub fn spin(iterations: u32) {
    for _ in 0..iterations {
        cortex_m::asm::nop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    idle_forever();
}
