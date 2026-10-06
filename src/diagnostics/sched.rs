// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Diagnostic helpers for scheduler state inspection.

use crate::sched::IDLE_THREAD_CTX;
use crate::thread::ThreadCtx;

pub fn traverse_idle_thread_fn<F>(mut f: F)
where
    F: FnMut(&ThreadCtx),
{
    crate::critical_section(|| unsafe {
        if !IDLE_THREAD_CTX.is_null() {
            f(&*IDLE_THREAD_CTX);
        }
    });
}
