// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if env::var_os("CARGO_FEATURE_HOST").is_some() {
        return;
    }

    let target = env::var("TARGET").expect("Cargo must set TARGET");
    match target.as_str() {
        "thumbv7m-none-eabi"
        | "thumbv7em-none-eabi"
        | "thumbv7em-none-eabihf"
        | "thumbv8m.main-none-eabi"
        | "thumbv8m.main-none-eabihf" => {}
        _ => panic!(
            "no rtsched platform is implemented for target `{target}`; \
             the Cortex-M port supports ARMv7-M, ARMv7E-M, and ARMv8-M Mainline \
             bare-metal targets (thumbv7m, thumbv7em, and thumbv8m.main); \
             enable `--features host` with a native target for host tests, \
             or add a hardware platform port"
        ),
    }
}
