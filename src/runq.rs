// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use cortex_m::interrupt;

use crate::ktimer::program_wait_ktimer;
use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::sched::CURRENT_THREAD_CTX;
use crate::thread::{ThreadCtx, ThreadState, cfs_sched_entity, thread_from_cfs_sched_entity};
use crate::waitq::{WaitQueueError, insert_wait_thread};

pub(crate) static CFS_RUN_QUEUE: RunQueue = RunQueue::new();

pub(crate) struct RunQueue {
    tree: UnsafeCell<RBTree<SchedEntity>>,
    priority_sum: UnsafeCell<u32>,
}

impl RunQueue {
    const fn new() -> Self {
        Self {
            tree: UnsafeCell::new(RBTree::new()),
            priority_sum: UnsafeCell::new(0),
        }
    }

    pub(crate) fn get(&self) -> *mut RBTree<SchedEntity> {
        self.tree.get()
    }

    pub(crate) fn priority_sum(&self) -> *mut u32 {
        self.priority_sum.get()
    }
}

unsafe impl Sync for RunQueue {}

/// Scheduler entity used as the tree node and ordering key.
///
/// `vruntime` is the primary key. When two entities have the same
/// `vruntime`, their addresses are used as a stable tie-breaker so insertion
/// order remains deterministic and the tree keeps a strict total ordering.
#[repr(C)]
pub struct SchedEntity {
    pub(crate) sched_tick_cnt: u64,
    /// Scheduler virtual runtime metric used as the red-black tree key.
    pub(crate) vruntime: u64,
    /// Scheduling priority, where the exact ordering is defined by the scheduler.
    pub priority: u32,
    pub(crate) rb_node: RbNode,
}

impl SchedEntity {
    /// Create a detached scheduler entity that can be inserted into a tree.
    pub const fn new(priority: u32) -> Self {
        Self {
            sched_tick_cnt: 0,
            vruntime: 0,
            priority,
            rb_node: RbNode::new(),
        }
    }

    /// Reset linkage so the entity can be reused or inserted into another tree.
    pub fn reset_links(&mut self) {
        self.rb_node.reset_links();
    }

    /// Return `true` if the entity is currently linked under another node.
    pub fn is_linked(&self) -> bool {
        self.rb_node.is_linked()
    }

    /// Return the scheduler virtual runtime used for run-queue ordering.
    pub fn vruntime(&self) -> u64 {
        self.vruntime
    }

    /// Return the scheduler tick count accumulated for this entity.
    pub fn sched_tick_cnt(&self) -> u64 {
        self.sched_tick_cnt
    }
}

unsafe impl RBTreeNode for SchedEntity {
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
                    .sub(offset_of!(SchedEntity, rb_node))
                    .cast::<SchedEntity>()
            }
        }
    }

    fn entity_of_const(node: *const RbNode) -> *const Self {
        if node.is_null() {
            ptr::null()
        } else {
            unsafe {
                (node as *const u8)
                    .sub(offset_of!(SchedEntity, rb_node))
                    .cast::<SchedEntity>()
            }
        }
    }

    unsafe fn cmp(a: *const Self, b: *const Self) -> core::cmp::Ordering {
        unsafe {
            match (*a).vruntime.cmp(&(*b).vruntime) {
                core::cmp::Ordering::Equal => (a as usize).cmp(&(b as usize)),
                other => other,
            }
        }
    }
}

pub(crate) fn thread_is_cfs(thread: *const ThreadCtx) -> bool {
    if thread.is_null() {
        return false;
    }

    unsafe { (*thread).is_cfs }
}

/// Traverse the scheduler-visible threads, including the running thread.
///
/// Pass `None` to get the CURRENT_THREAD_CTX running thread when one exists; otherwise
/// this returns the first queued thread. Pass the previously returned thread to
/// get the next entry. After the running thread, traversal continues through
/// the run queue in ascending vruntime order. Returns `None` after the last
/// queued thread.
///
/// # Safety
///
/// The caller must ensure that any provided thread pointer still refers to a
/// valid thread control block and that the run queue is not concurrently
/// mutated in a way that invalidates the traversal step.
pub unsafe fn traverse_run_queue(cursor: Option<*mut ThreadCtx>) -> Option<*mut ThreadCtx> {
    unsafe {
        let tree = &*CFS_RUN_QUEUE.get();
        match cursor {
            None => {
                if !CURRENT_THREAD_CTX.is_null() {
                    Some(CURRENT_THREAD_CTX)
                } else {
                    let first = tree.first();
                    if first.is_null() {
                        None
                    } else {
                        Some(thread_from_cfs_sched_entity(first))
                    }
                }
            }
            Some(thread) if thread == CURRENT_THREAD_CTX => {
                let first = tree.first();
                if first.is_null() {
                    None
                } else {
                    Some(thread_from_cfs_sched_entity(first))
                }
            }
            Some(thread) => {
                let next = tree.next(cfs_sched_entity(thread));
                if next.is_null() {
                    None
                } else {
                    Some(thread_from_cfs_sched_entity(next))
                }
            }
        }
    }
}

/// Reset the scheduler run queue to an empty state.
pub(crate) unsafe fn init_cfs_rq() {
    unsafe {
        *CFS_RUN_QUEUE.get() = RBTree::new();
        *CFS_RUN_QUEUE.priority_sum() = 0;
    }
}

/// Align a detached entity's vruntime and sched_tick_cnt with the left-most queued entity.
///
/// If the run queue is empty, the entity keeps its current vruntime.
pub(crate) unsafe fn update_from_leftmost(entity: *mut SchedEntity, priority_sum: u32) {
    if entity.is_null() {
        return;
    }

    unsafe {
        let leftmost = (*CFS_RUN_QUEUE.get()).first();
        if !leftmost.is_null() {
            (*entity).vruntime = (*leftmost).vruntime;
            (*entity).sched_tick_cnt =
                (*entity).vruntime * priority_sum as u64 / (*entity).priority as u64;
        }
    }
}

/// Enqueue a thread into the scheduler run queue.
///
/// The thread's scheduler entity vruntime field is used as the red-black tree key.
pub unsafe fn enqueue_thread(thread: *mut ThreadCtx) {
    unsafe {
        (*thread).state = ThreadState::Ready;
        let entity = cfs_sched_entity(thread);
        (*entity).reset_links();
        update_from_leftmost(entity, *CFS_RUN_QUEUE.priority_sum());
        (*CFS_RUN_QUEUE.get()).insert(entity);
        *CFS_RUN_QUEUE.priority_sum() += (*entity).priority;
    }
}

/// Remove a thread from the scheduler run queue if it is currently queued.
#[allow(dead_code)]
pub unsafe fn dequeue_thread(thread: *mut ThreadCtx) {
    unsafe {
        if (*thread).state == ThreadState::Ready {
            let entity = cfs_sched_entity(thread);
            (*CFS_RUN_QUEUE.get()).remove(entity);
            *CFS_RUN_QUEUE.priority_sum() -= (*entity).priority;
        }
    }
}

pub fn dequeue_runq_to_waitq(thread: *mut ThreadCtx) -> Result<(), WaitQueueError> {
    interrupt::free(|_| unsafe {
        if thread.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        let entity = cfs_sched_entity(thread);
        if (*thread).state == ThreadState::Ready {
            (*CFS_RUN_QUEUE.get()).remove(entity);
        }
        (*thread).state = ThreadState::Waiting;

        insert_wait_thread(thread);
        (*CFS_RUN_QUEUE.priority_sum()) -= (*entity).priority;
        program_wait_ktimer(true);

        Ok(())
    })
}
