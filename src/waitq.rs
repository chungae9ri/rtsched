// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use crate::critical_section;
use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::thread::{ThreadCtx, cfs_wait_entity, rt_wait_entity, thread_from_wait_entity};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WaitQueueError {
    NotFound,
}

pub(crate) struct WaitQueue {
    tree: UnsafeCell<RBTree<WaitEntity>>,
}

impl WaitQueue {
    const fn new() -> Self {
        Self {
            tree: UnsafeCell::new(RBTree::new()),
        }
    }

    pub(crate) fn get(&self) -> *mut RBTree<WaitEntity> {
        self.tree.get()
    }
}

unsafe impl Sync for WaitQueue {}

pub(crate) static WAIT_QUEUE: WaitQueue = WaitQueue::new();

pub struct WaitEntity {
    pub wake_at: u64,
    pub waitevt: u32,
    rb_node: RbNode,
}

impl WaitEntity {
    pub const fn new() -> Self {
        Self {
            wake_at: 0,
            waitevt: 0,
            rb_node: RbNode::new(),
        }
    }

    pub fn set_wake_after(&mut self, now_ticks: u64, wait_ticks: u32) {
        self.wake_at = now_ticks.saturating_add(u64::from(wait_ticks));
    }

    pub fn remaining_at(&self, now_ticks: u64) -> u32 {
        self.wake_at
            .saturating_sub(now_ticks)
            .min(u64::from(u32::MAX)) as u32
    }

    pub fn is_expired_at(&self, now_ticks: u64) -> bool {
        self.wake_at <= now_ticks
    }

    pub fn reset_links(&mut self) {
        self.rb_node.reset_links();
    }
}

unsafe impl RBTreeNode for WaitEntity {
    fn node(entity: *mut Self) -> *mut RbNode {
        if entity.is_null() {
            ptr::null_mut()
        } else {
            unsafe { ptr::addr_of_mut!((*entity).rb_node) }
        }
    }

    fn entity_of(node: *mut RbNode) -> *mut Self {
        if node.is_null() {
            ptr::null_mut()
        } else {
            unsafe {
                (node as *mut u8)
                    .sub(offset_of!(WaitEntity, rb_node))
                    .cast::<WaitEntity>()
            }
        }
    }

    fn entity_of_const(node: *const RbNode) -> *const Self {
        if node.is_null() {
            ptr::null()
        } else {
            unsafe {
                (node as *const u8)
                    .sub(offset_of!(WaitEntity, rb_node))
                    .cast::<WaitEntity>()
            }
        }
    }

    unsafe fn cmp(a: *const Self, b: *const Self) -> core::cmp::Ordering {
        unsafe {
            match (*a).wake_at.cmp(&(*b).wake_at) {
                core::cmp::Ordering::Equal => match (*a).waitevt.cmp(&(*b).waitevt) {
                    core::cmp::Ordering::Equal => (a as usize).cmp(&(b as usize)),
                    other => other,
                },
                other => other,
            }
        }
    }
}

/// Traverse waiting threads in ascending wait-time order.
///
/// Pass `None` to return the first waiting thread. Pass the previously returned
/// thread to return the next one. Returns `None` after the final waiting thread.
///
/// # Safety
///
/// The caller must ensure that any provided thread pointer remains valid and
/// that the wait queue is not mutated during traversal.
pub(crate) unsafe fn traverse_wait_queue(cursor: Option<*mut ThreadCtx>) -> Option<*mut ThreadCtx> {
    unsafe {
        let tree = &*WAIT_QUEUE.get();
        let entity = match cursor {
            None => tree.first(),
            Some(thread) => tree.next(wait_entity(thread)),
        };

        if entity.is_null() {
            None
        } else {
            Some(thread_from_wait_entity(entity))
        }
    }
}

/// Visit waiting threads without exposing raw traversal cursors.
pub fn traverse_wait_queue_fn<F>(mut f: F)
where
    F: FnMut(&ThreadCtx),
{
    critical_section(|| unsafe {
        let mut cursor = traverse_wait_queue(None);
        while let Some(thread) = cursor {
            f(&*thread);
            cursor = traverse_wait_queue(Some(thread));
        }
    });
}

pub(crate) unsafe fn wait_entity(thread: *mut ThreadCtx) -> *mut WaitEntity {
    unsafe {
        if (*thread).is_cfs {
            cfs_wait_entity(thread)
        } else {
            rt_wait_entity(thread)
        }
    }
}

pub(crate) unsafe fn pop_expired_wait_thread(now_ticks: u64) -> *mut ThreadCtx {
    unsafe {
        let tree = &mut *WAIT_QUEUE.get();
        let first = tree.first();
        if first.is_null() || !(*first).is_expired_at(now_ticks) {
            return ptr::null_mut();
        }

        let Some(entity) = tree.pop_first() else {
            return ptr::null_mut();
        };

        thread_from_wait_entity(entity as *mut WaitEntity)
    }
}

pub(crate) unsafe fn insert_wait_thread(thread: *mut ThreadCtx) {
    unsafe {
        let wait_entity = wait_entity(thread);
        let tree = &mut *WAIT_QUEUE.get();
        debug_assert!(
            !tree.contains(wait_entity.cast_const()),
            "wait entity is already queued"
        );
        (*wait_entity).reset_links();
        tree.insert(wait_entity);
    }
}

pub(crate) unsafe fn remove_wait_thread(thread: *mut ThreadCtx) {
    unsafe {
        let wait_entity = wait_entity(thread);
        (*WAIT_QUEUE.get()).remove(wait_entity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::runq::SchedEntity;
    use crate::thread::{CfsThread, ThreadState};
    use std::vec::Vec;

    fn reset_wait_queue() {
        unsafe {
            *WAIT_QUEUE.get() = RBTree::new();
        }
    }

    fn cfs_thread(name: &'static str, wake_at: u64, waitevt: u32) -> CfsThread {
        let mut thread = CfsThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 0,
                name,
                state: ThreadState::Waiting,
                is_cfs: true,
            },
            wait_entity: WaitEntity::new(),
            sched_entity: SchedEntity::new(1),
        };
        thread.wait_entity.wake_at = wake_at;
        thread.wait_entity.waitevt = waitevt;
        thread
    }

    fn collect_wake_at() -> Vec<u64> {
        let mut deadlines = Vec::new();
        let tree = unsafe { &*WAIT_QUEUE.get() };
        let mut entity = tree.first();

        while !entity.is_null() {
            unsafe {
                deadlines.push((*entity).wake_at);
            }
            entity = tree.next(entity);
        }

        deadlines
    }

    fn collect_remaining(now_ticks: u64) -> Vec<u32> {
        let mut remaining = Vec::new();
        let tree = unsafe { &*WAIT_QUEUE.get() };
        let mut entity = tree.first();

        while !entity.is_null() {
            unsafe {
                remaining.push((*entity).remaining_at(now_ticks));
            }
            entity = tree.next(entity);
        }

        remaining
    }

    #[test]
    fn wait_entities_order_by_ticks_then_event() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut first = cfs_thread("first", 10, 2);
        let mut second = cfs_thread("second", 5, 9);
        let mut third = cfs_thread("third", 10, 1);

        unsafe {
            insert_wait_thread(&mut first.thread);
            insert_wait_thread(&mut second.thread);
            insert_wait_thread(&mut third.thread);
        }

        assert_eq!(collect_wake_at(), [5, 10, 10]);
        let mut names = Vec::new();
        traverse_wait_queue_fn(|thread| names.push(thread.name));
        assert_eq!(names, ["second", "third", "first"]);
        let first_thread = unsafe { traverse_wait_queue(None).unwrap() };
        let second_thread = unsafe { traverse_wait_queue(Some(first_thread)).unwrap() };
        let third_thread = unsafe { traverse_wait_queue(Some(second_thread)).unwrap() };

        unsafe {
            assert_eq!((*first_thread).name, "second");
            assert_eq!((*second_thread).name, "third");
            assert_eq!((*third_thread).name, "first");
            assert!(traverse_wait_queue(Some(third_thread)).is_none());
        }
    }

    #[test]
    fn remaining_time_uses_absolute_wake_deadline() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut first = cfs_thread("first", 3, 0);
        let mut second = cfs_thread("second", 12, 0);
        let mut third = cfs_thread("third", 7, 0);

        unsafe {
            insert_wait_thread(&mut first.thread);
            insert_wait_thread(&mut second.thread);
            insert_wait_thread(&mut third.thread);
        }

        assert_eq!(first.wait_entity.wake_at, 3);
        assert_eq!(second.wait_entity.wake_at, 12);
        assert_eq!(third.wait_entity.wake_at, 7);
        assert_eq!(collect_wake_at(), [3, 7, 12]);
        assert_eq!(collect_remaining(5), [0, 2, 7]);
    }

    #[test]
    fn pop_expired_wait_thread_only_pops_zero_tick_threads() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut expired = cfs_thread("expired", 0, 0);
        let mut pending = cfs_thread("pending", 4, 0);

        unsafe {
            insert_wait_thread(&mut pending.thread);
            insert_wait_thread(&mut expired.thread);
        }

        let popped = unsafe { pop_expired_wait_thread(0) };
        assert!(ptr::eq(popped, &expired.thread));
        assert_eq!(unsafe { pop_expired_wait_thread(0) }, ptr::null_mut());
        assert_eq!(collect_wake_at(), [4]);
    }

    #[test]
    fn remove_wait_thread_detaches_entity() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut first = cfs_thread("first", 1, 0);
        let mut second = cfs_thread("second", 2, 0);

        unsafe {
            insert_wait_thread(&mut first.thread);
            insert_wait_thread(&mut second.thread);
            remove_wait_thread(&mut first.thread);
        }

        assert_eq!(collect_wake_at(), [2]);
        assert!(!first.wait_entity.rb_node.is_linked());
    }
}
