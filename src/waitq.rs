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
        let tree = &*WAIT_QUEUE.get();
        let mut entity = tree.first();

        while !entity.is_null() {
            let next = tree.next(entity);
            (*entity).wait_ticks = (*entity).wait_ticks.saturating_sub(elapsed);
            entity = next;
        }
    }
}

pub(crate) unsafe fn pop_first_wait_thread() -> *mut ThreadCtx {
    unsafe {
        let tree = &mut *WAIT_QUEUE.get();
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
