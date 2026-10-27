// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::sync::SyncType;
use crate::thread::{
    ThreadHandle, ThreadKind, ThreadState, cfs_wait_entity, rt_wait_entity,
    thread_handle_from_wait_entity,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitQueueError {
    NotFound,
}

pub(crate) struct WaitQueue {
    tree: UnsafeCell<RBTree<WaitEntity>>,
    suspended: UnsafeCell<SuspendedWaitList>,
}

impl WaitQueue {
    const fn new() -> Self {
        Self {
            tree: UnsafeCell::new(RBTree::new()),
            suspended: UnsafeCell::new(SuspendedWaitList::new()),
        }
    }

    pub(crate) fn get(&self) -> *mut RBTree<WaitEntity> {
        self.tree.get()
    }

    /// Visit finite waits in deadline order, followed by indefinite waits in FIFO order.
    /// The caller must serialize access and provide a live, queued cursor.
    pub(crate) unsafe fn next_wait_entity(&self, cursor: Option<ThreadHandle>) -> *mut WaitEntity {
        unsafe {
            let entity = match cursor {
                None => (*self.get()).first(),
                Some(thread) if (*thread.as_ptr()).state == ThreadState::Suspended => {
                    return (*wait_entity(thread)).suspended_next;
                }
                Some(thread) => (*self.get()).next(wait_entity(thread)),
            };
            if entity.is_null() {
                (*self.suspended.get()).head
            } else {
                entity
            }
        }
    }

    #[cfg(not(target_arch = "arm"))]
    pub(crate) unsafe fn reset_for_test(&self) {
        unsafe {
            ptr::write(self.tree.get(), RBTree::new());
            ptr::write(self.suspended.get(), SuspendedWaitList::new());
        }
    }
}

unsafe impl Sync for WaitQueue {}

pub(crate) static WAIT_QUEUE: WaitQueue = WaitQueue::new();

// Access is serialized with the timeout tree. Nodes belong to exactly one
// blocked-thread index; the per-object urgency tree uses separate SyncEntity links.
struct SuspendedWaitList {
    head: *mut WaitEntity,
    tail: *mut WaitEntity,
}

impl SuspendedWaitList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
        }
    }

    unsafe fn push_back(&mut self, entity: *mut WaitEntity) {
        unsafe {
            debug_assert!(entity != self.head && entity != self.tail);
            debug_assert!((*entity).suspended_prev.is_null());
            debug_assert!((*entity).suspended_next.is_null());
            (*entity).suspended_prev = self.tail;
            if self.tail.is_null() {
                self.head = entity;
            } else {
                (*self.tail).suspended_next = entity;
            }
            self.tail = entity;
        }
    }

    unsafe fn remove(&mut self, entity: *mut WaitEntity) {
        unsafe {
            let prev = (*entity).suspended_prev;
            let next = (*entity).suspended_next;
            if prev.is_null() {
                debug_assert_eq!(self.head, entity);
                self.head = next;
            } else {
                (*prev).suspended_next = next;
            }
            if next.is_null() {
                debug_assert_eq!(self.tail, entity);
                self.tail = prev;
            } else {
                (*next).suspended_prev = prev;
            }
            (*entity).suspended_prev = ptr::null_mut();
            (*entity).suspended_next = ptr::null_mut();
        }
    }
}

pub struct WaitEntity {
    pub wake_at: u64,
    pub waitevt: Option<SyncType>,
    rb_node: RbNode,
    suspended_prev: *mut WaitEntity,
    suspended_next: *mut WaitEntity,
}

impl WaitEntity {
    pub const fn new() -> Self {
        Self {
            wake_at: 0,
            waitevt: None,
            rb_node: RbNode::new(),
            suspended_prev: ptr::null_mut(),
            suspended_next: ptr::null_mut(),
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

pub(crate) unsafe fn wait_entity(thread: ThreadHandle) -> *mut WaitEntity {
    unsafe {
        match (*thread.as_ptr()).kind {
            ThreadKind::Cfs => cfs_wait_entity(thread),
            ThreadKind::Rt => rt_wait_entity(thread),
            ThreadKind::Idle => panic!("idle thread has no wait entity"),
        }
    }
}

pub(crate) unsafe fn pop_expired_wait_thread(now_ticks: u64) -> Option<ThreadHandle> {
    unsafe {
        let tree = &mut *WAIT_QUEUE.get();
        let first = tree.first();
        if first.is_null() || !(*first).is_expired_at(now_ticks) {
            return None;
        }

        let Some(entity) = tree.pop_first() else {
            return None;
        };

        Some(thread_handle_from_wait_entity(entity as *mut WaitEntity))
    }
}

pub(crate) unsafe fn insert_wait_thread(thread: ThreadHandle) {
    unsafe {
        let wait_entity = wait_entity(thread);
        if (*wait_entity).wake_at == u64::MAX {
            (*thread.as_ptr()).set_state(ThreadState::Suspended);
            (*WAIT_QUEUE.suspended.get()).push_back(wait_entity);
            return;
        }
        (*thread.as_ptr()).set_state(ThreadState::Waiting);
        let tree = &mut *WAIT_QUEUE.get();
        debug_assert!(
            !tree.contains(wait_entity.cast_const()),
            "wait entity is already queued"
        );
        (*wait_entity).reset_links();
        tree.insert(wait_entity);
    }
}

pub(crate) unsafe fn remove_wait_thread(thread: ThreadHandle) {
    unsafe {
        let wait_entity = wait_entity(thread);
        if (*thread.as_ptr()).state == ThreadState::Suspended {
            (*WAIT_QUEUE.suspended.get()).remove(wait_entity);
        } else {
            (*WAIT_QUEUE.get()).remove(wait_entity);
        }
    }
}

#[cfg(not(target_arch = "arm"))]
#[path = "../tests/support/waitq.rs"]
pub mod test_support;
