// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Diagnostic helpers for CFS run-queue inspection.

use crate::runq::CFS_RUN_QUEUE;
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS};
use crate::thread::{
    CfsThread, ThreadHandle, cfs_sched_entity, cfs_thread_from_handle,
    thread_handle_from_cfs_sched_entity,
};

/// Traverse the CFS scheduler-visible threads, including the running CFS thread.
///
/// Pass `None` to get the CURRENT_THREAD_CTX running thread when it is a CFS
/// thread; otherwise this returns the first queued CFS thread. Pass the
/// previously returned thread to get the next entry. After the running CFS
/// thread, traversal continues through the run queue in ascending vruntime
/// order. Returns `None` after the last queued CFS thread.
///
/// # Safety
///
/// The caller must ensure that any provided thread pointer still refers to a
/// valid thread control block and that the run queue is not concurrently
/// mutated in a way that invalidates the traversal step.
unsafe fn traverse_run_queue(cursor: Option<ThreadHandle>) -> Option<ThreadHandle> {
    unsafe {
        let tree = &*CFS_RUN_QUEUE.get();
        match cursor {
            None => {
                if CURRENT_THREAD_IS_CFS && !CURRENT_THREAD_CTX.is_null() {
                    Some(ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX))
                } else {
                    let first = tree.first();
                    if first.is_null() {
                        None
                    } else {
                        Some(thread_handle_from_cfs_sched_entity(first))
                    }
                }
            }
            Some(thread) if thread.as_ptr() == CURRENT_THREAD_CTX => {
                let first = tree.first();
                if first.is_null() {
                    None
                } else {
                    Some(thread_handle_from_cfs_sched_entity(first))
                }
            }
            Some(thread) => {
                let next = tree.next(cfs_sched_entity(thread));
                if next.is_null() {
                    None
                } else {
                    Some(thread_handle_from_cfs_sched_entity(next))
                }
            }
        }
    }
}

/// Visit scheduler-visible CFS threads without exposing raw traversal cursors.
pub fn traverse_run_queue_fn<F>(mut f: F)
where
    F: FnMut(&CfsThread),
{
    crate::critical_section(|| unsafe {
        let mut cursor = traverse_run_queue(None);
        while let Some(thread) = cursor {
            f(&*cfs_thread_from_handle(thread));
            cursor = traverse_run_queue(Some(thread));
        }
    });
}
