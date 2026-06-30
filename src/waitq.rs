// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

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
    pub wait_ticks: u32,
    pub waitevt: u32,
    rb_node: RbNode,
}

impl WaitEntity {
    pub const fn new() -> Self {
        Self {
            wait_ticks: 0,
            waitevt: 0,
            rb_node: RbNode::new(),
        }
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
            match (*a).wait_ticks.cmp(&(*b).wait_ticks) {
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
pub unsafe fn traverse_wait_queue(cursor: Option<*mut ThreadCtx>) -> Option<*mut ThreadCtx> {
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

pub(crate) unsafe fn wait_entity(thread: *mut ThreadCtx) -> *mut WaitEntity {
    unsafe {
        if (*thread).is_cfs {
            cfs_wait_entity(thread)
        } else {
            rt_wait_entity(thread)
        }
    }
}

pub(crate) unsafe fn advance_wait_queue(elapsed: u32) {
    unsafe {
        let tree = &mut *WAIT_QUEUE.get();
        let mut advanced = RBTree::new();

        while let Some(entity) = tree.pop_first() {
            let entity = entity as *mut WaitEntity;
            (*entity).wait_ticks = (*entity).wait_ticks.saturating_sub(elapsed);
            advanced.insert(entity);
        }

        *tree = advanced;
    }
}

pub(crate) unsafe fn pop_expired_wait_thread() -> *mut ThreadCtx {
    unsafe {
        let tree = &mut *WAIT_QUEUE.get();
        let first = tree.first();
        if first.is_null() || (*first).wait_ticks != 0 {
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
        (*wait_entity).reset_links();
        (*WAIT_QUEUE.get()).insert(wait_entity);
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
    use crate::runq::SchedEntity;
    use crate::thread::{CfsThread, ThreadState};
    use std::sync::Mutex;
    use std::vec::Vec;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_wait_queue() {
        unsafe {
            *WAIT_QUEUE.get() = RBTree::new();
        }
    }

    fn cfs_thread(name: &'static str, wait_ticks: u32, waitevt: u32) -> CfsThread {
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
        thread.wait_entity.wait_ticks = wait_ticks;
        thread.wait_entity.waitevt = waitevt;
        thread
    }

    fn collect_wait_ticks() -> Vec<u32> {
        let mut ticks = Vec::new();
        let tree = unsafe { &*WAIT_QUEUE.get() };
        let mut entity = tree.first();

        while !entity.is_null() {
            unsafe {
                ticks.push((*entity).wait_ticks);
            }
            entity = tree.next(entity);
        }

        ticks
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

        assert_eq!(collect_wait_ticks(), [5, 10, 10]);
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
    fn advance_wait_queue_saturates_and_reorders() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_wait_queue();
        let mut first = cfs_thread("first", 3, 0);
        let mut second = cfs_thread("second", 12, 0);
        let mut third = cfs_thread("third", 7, 0);

        unsafe {
            insert_wait_thread(&mut first.thread);
            insert_wait_thread(&mut second.thread);
            insert_wait_thread(&mut third.thread);
            advance_wait_queue(5);
        }

        assert_eq!(first.wait_entity.wait_ticks, 0);
        assert_eq!(second.wait_entity.wait_ticks, 7);
        assert_eq!(third.wait_entity.wait_ticks, 2);
        assert_eq!(collect_wait_ticks(), [0, 2, 7]);
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

        let popped = unsafe { pop_expired_wait_thread() };
        assert!(ptr::eq(popped, &mut expired.thread));
        assert_eq!(unsafe { pop_expired_wait_thread() }, ptr::null_mut());
        assert_eq!(collect_wait_ticks(), [4]);
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

        assert_eq!(collect_wait_ticks(), [2]);
        assert!(!first.wait_entity.rb_node.is_linked());
    }
}
