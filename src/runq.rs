// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use crate::critical_section;
use crate::ktimer::program_wait_ktimer;
use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::thread::{ThreadHandle, ThreadState, cfs_sched_entity};
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
///
/// CFS priority is inverse-numeric: `1` is the most favored priority and larger
/// values are less favored. A larger numeric priority charges more `vruntime`
/// for the same elapsed time, so the scheduler will tend to choose it less
/// often than a lower numeric priority.
#[repr(C)]
pub struct SchedEntity {
    pub(crate) sched_tick_cnt: u64,
    /// Scheduler virtual runtime metric used as the red-black tree key.
    pub(crate) vruntime: u64,
    /// Non-zero inverse-numeric priority. Lower values are favored.
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
    #[allow(dead_code)]
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

/// Calculate how much `vruntime` to charge for elapsed CFS execution.
///
/// Lower numeric CFS priority values are favored because this formula charges
/// less `vruntime` for the same elapsed time. The scheduler selects the CFS
/// thread with the smallest `vruntime`.
pub(crate) fn cfs_vruntime_delta(elapsed_ticks: u64, priority: u32, priority_sum: u32) -> u64 {
    debug_assert!(priority != 0, "CFS thread priority must be non-zero");

    if priority_sum == 0 {
        return 0;
    }

    elapsed_ticks * u64::from(priority) / u64::from(priority_sum)
}

fn cfs_sched_ticks_from_vruntime(vruntime: u64, priority: u32, priority_sum: u32) -> u64 {
    debug_assert!(priority != 0, "CFS thread priority must be non-zero");

    if priority_sum == 0 {
        return 0;
    }

    vruntime * u64::from(priority_sum) / u64::from(priority)
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
pub(crate) unsafe fn update_from_leftmost(entity: *mut SchedEntity) {
    if entity.is_null() {
        return;
    }

    unsafe {
        let priority_sum = *CFS_RUN_QUEUE.priority_sum();
        let priority = (*entity).priority;
        let leftmost = (*CFS_RUN_QUEUE.get()).first();

        if !leftmost.is_null() {
            (*entity).vruntime = (*leftmost).vruntime;
            (*entity).sched_tick_cnt =
                cfs_sched_ticks_from_vruntime((*entity).vruntime, priority, priority_sum);
        }
    }
}

/// Enqueue a thread into the scheduler run queue.
///
/// The thread's scheduler entity vruntime field is used as the red-black tree key.
///
/// # Safety
///
/// `thread` must be non-null and must point to the `ThreadCtx` embedded in a
/// live `CfsThread` whose storage outlives its run-queue membership. Its
/// scheduler entity must not already be linked into the run queue.
///
/// Call this only after the CFS run queue has been initialized and while
/// scheduler state is not being concurrently modified.
pub unsafe fn enqueue_thread(thread: ThreadHandle) {
    unsafe {
        let thread_ptr = thread.as_ptr();
        (*thread_ptr).set_state(ThreadState::Ready);
        let entity = cfs_sched_entity(thread);
        let tree = &mut *CFS_RUN_QUEUE.get();
        debug_assert!(
            !tree.contains(entity.cast_const()),
            "sched entity is already queued"
        );
        (*entity).reset_links();
        update_from_leftmost(entity);
        tree.insert(entity);
        *CFS_RUN_QUEUE.priority_sum() += (*entity).priority;
    }
}

/// # Safety
///
/// `thread` must be non-null, currently waiting, and must point to the
/// `ThreadCtx` embedded in a live `CfsThread`. Its wait entity must already
/// have been removed from the wait queue, and its scheduler entity must not be
/// linked into the run queue.
///
/// Call this only from the wait-timer dispatch path while scheduler state is
/// serialized.
pub unsafe fn enqueue_runq_from_waitq(thread: ThreadHandle) {
    unsafe {
        let thread_ptr = thread.as_ptr();
        debug_assert!((*thread_ptr).is_cfs(), "thread must be a CfsThread");

        let entity = cfs_sched_entity(thread);
        let priority_sum = (*CFS_RUN_QUEUE.priority_sum()).saturating_add((*entity).priority);
        let tree = &mut *CFS_RUN_QUEUE.get();
        debug_assert!(
            !tree.contains(entity.cast_const()),
            "sched entity is already queued"
        );

        (*entity).reset_links();
        *CFS_RUN_QUEUE.priority_sum() = priority_sum;
        (*thread_ptr).set_state(ThreadState::Ready);

        // Update cfs_rq with priority_sum
        let mut updated = RBTree::new();

        while let Some(queued_entity) = tree.pop_first() {
            let priority_sum = *CFS_RUN_QUEUE.priority_sum();
            let priority = queued_entity.priority;

            queued_entity.sched_tick_cnt =
                cfs_sched_ticks_from_vruntime(queued_entity.vruntime, priority, priority_sum);

            updated.insert(queued_entity);
        }

        *tree = updated;

        update_from_leftmost(entity);
        (*CFS_RUN_QUEUE.get()).insert(entity);
    }
}

pub(crate) fn dequeue_runq_to_waitq(thread: ThreadHandle) -> Result<(), WaitQueueError> {
    critical_section(|| unsafe {
        let thread_ptr = thread.as_ptr();
        debug_assert!((*thread_ptr).is_cfs(), "thread must be a CfsThread");

        let entity = cfs_sched_entity(thread);
        // If the thread is Running, it is not in the runq.
        if (*thread_ptr).state == ThreadState::Ready {
            (*CFS_RUN_QUEUE.get()).remove(entity);
        }
        (*thread_ptr).set_state(ThreadState::Waiting);

        insert_wait_thread(thread);
        let priority_sum = (*CFS_RUN_QUEUE.priority_sum()).saturating_sub((*entity).priority);
        *CFS_RUN_QUEUE.priority_sum() = priority_sum;
        program_wait_ktimer();

        Ok(())
    })
}

#[cfg(not(target_arch = "arm"))]
#[path = "../tests/support/runq.rs"]
pub mod test_support;
