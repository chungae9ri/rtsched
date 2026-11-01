// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

#[cfg(all(target_arch = "arm", not(feature = "host")))]
pub mod cm;
#[cfg(feature = "host")]
pub mod host;
pub(crate) mod platform;
