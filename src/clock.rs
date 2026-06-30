// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

static mut SYS_CLK_FREQ: u32 = 0;

pub fn sys_clk_freq() -> u32 {
    unsafe { core::ptr::read_volatile(&raw const SYS_CLK_FREQ) }
}

pub fn update_sys_clk_freq(freq: u32) {
    #[cfg(target_arch = "arm")]
    cortex_m::interrupt::free(|_| unsafe {
        core::ptr::write_volatile(&raw mut SYS_CLK_FREQ, freq);
    });

    #[cfg(not(target_arch = "arm"))]
    unsafe {
        core::ptr::write_volatile(&raw mut SYS_CLK_FREQ, freq);
    }
}

pub fn ticks_per_ms() -> u32 {
    sys_clk_freq() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn update_sys_clk_freq_updates_ticks_per_ms() {
        let _guard = TEST_LOCK.lock().unwrap();

        update_sys_clk_freq(12_000_000);

        assert_eq!(sys_clk_freq(), 12_000_000);
        assert_eq!(ticks_per_ms(), 12_000);
    }

    #[test]
    fn ticks_per_ms_truncates_sub_millisecond_remainder() {
        let _guard = TEST_LOCK.lock().unwrap();

        update_sys_clk_freq(12_345);

        assert_eq!(ticks_per_ms(), 12);
    }
}
