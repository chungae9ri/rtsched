// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if env::var_os("CARGO_FEATURE_HOST").is_some() {
        return;
    }

    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("arm") => {}
        _ => panic!(
            "no rtsched platform is implemented for this target; \
             enable `--features host` with a native target for host tests, \
             or add a hardware platform port"
        ),
    }
}
