use std::sync::Mutex;

use rtsched::{sys_clk_freq, ticks_per_ms, update_sys_clk_freq};

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
