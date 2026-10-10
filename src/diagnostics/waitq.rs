// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Diagnostic helpers for wait-queue inspection.

use crate::thread::{
    ThreadHandle, ThreadRef, thread_handle_from_wait_entity, thread_ref_from_handle,
};
use crate::waitq::{WAIT_QUEUE, wait_entity};

/// Traverse waiting threads in ascending wait-time order.
///
/// Pass `None` to return the first waiting thread. Pass the previously returned
/// thread to return the next one. Returns `None` after the final waiting thread.
///
/// # Safety
///
/// The caller must ensure that any provided thread pointer remains valid and
/// that the wait queue is not mutated during traversal.
pub(crate) unsafe fn traverse_wait_queue(cursor: Option<ThreadHandle>) -> Option<ThreadHandle> {
    unsafe {
        let tree = &*WAIT_QUEUE.get();
        let entity = match cursor {
            None => tree.first(),
            Some(thread) => tree.next(wait_entity(thread)),
        };

        if entity.is_null() {
            None
        } else {
            Some(thread_handle_from_wait_entity(entity))
        }
    }
}

/// Visit waiting threads without exposing raw traversal cursors.
pub fn traverse_wait_queue_fn<F>(mut f: F)
where
    F: for<'a> FnMut(ThreadRef<'a>),
{
    crate::critical_section(|| unsafe {
        let mut cursor = traverse_wait_queue(None);
        while let Some(thread) = cursor {
            f(thread_ref_from_handle(thread));
            cursor = traverse_wait_queue(Some(thread));
        }
    });
}
