// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use cortex_m::peripheral::{DCB, DWT};

use crate::clock::ticks_per_ms;
use crate::critical_section;

static mut DWT_ELAPSE_START_CYCLE: u32 = 0;

fn dwt_elapse_start_cycle() -> u32 {
    unsafe { core::ptr::read_volatile(&raw const DWT_ELAPSE_START_CYCLE) }
}

fn msec_from_cycles(cycles: u32) -> u32 {
    let ticks = ticks_per_ms();

    if ticks == 0 {
        return 0;
    }

    cycles / ticks
}

/// Enable the DWT cycle counter and reset the elapsed-time baseline.
///
/// Returns `false` when the Cortex-M implementation does not provide a DWT
/// cycle counter or when enabling it did not take effect.
pub fn init_dwt_cycle_counter(dcb: &mut DCB, dwt: &mut DWT) -> bool {
    critical_section(|| unsafe {
        if !DWT::has_cycle_counter() {
            return false;
        }

        dcb.enable_trace();
        dwt.set_cycle_count(0);
        dwt.enable_cycle_counter();

        if !DWT::cycle_counter_enabled() {
            return false;
        }

        core::ptr::write_volatile(&raw mut DWT_ELAPSE_START_CYCLE, DWT::cycle_count());
        true
    })
}

pub fn dwt_cycle_count() -> u32 {
    DWT::cycle_count()
}

/// Reset the baseline used by `get_elapse_cycles` and `get_elapse_msec`.
pub fn reset_elapse_counter() {
    critical_section(|| unsafe {
        core::ptr::write_volatile(&raw mut DWT_ELAPSE_START_CYCLE, DWT::cycle_count());
    });
}

/// Return elapsed DWT cycles since the last DWT elapsed-counter reset.
///
/// The subtraction is wrapping, so the value is valid for intervals shorter
/// than one full 32-bit DWT counter wrap.
pub fn get_elapse_cycles() -> u32 {
    DWT::cycle_count().wrapping_sub(dwt_elapse_start_cycle())
}

/// Return elapsed milliseconds since the last DWT elapsed-counter reset.
///
/// `update_sys_clk_freq` must be called before using this conversion.
pub fn get_elapse_msec() -> u32 {
    msec_from_cycles(get_elapse_cycles())
}

pub fn get_elapse_msec_since(start_cycle: u32) -> u32 {
    msec_from_cycles(DWT::cycle_count().wrapping_sub(start_cycle))
}
