// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::sync::SyncType;
use crate::thread::{
    ThreadHandle, ThreadKind, cfs_wait_entity, rt_wait_entity, thread_handle_from_wait_entity,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub waitevt: Option<SyncType>,
    rb_node: RbNode,
}

impl WaitEntity {
    pub const fn new() -> Self {
        Self {
            wake_at: 0,
            waitevt: None,
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
        (*WAIT_QUEUE.get()).remove(wait_entity);
    }
}

#[cfg(not(target_arch = "arm"))]
#[path = "../tests/support/waitq.rs"]
pub mod test_support;
