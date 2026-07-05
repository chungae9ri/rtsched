// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use crate::critical_section;
use crate::ktimer::program_wait_ktimer;
use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS};
use crate::thread::{
    CfsThread, ThreadCtx, ThreadState, cfs_sched_entity, thread_from_cfs_sched_entity,
};
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

pub(crate) fn thread_is_cfs(thread: *const ThreadCtx) -> bool {
    if thread.is_null() {
        return false;
    }

    unsafe { (*thread).is_cfs }
}

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
pub(crate) unsafe fn traverse_run_queue(cursor: Option<*mut ThreadCtx>) -> Option<*mut ThreadCtx> {
    unsafe {
        let tree = &*CFS_RUN_QUEUE.get();
        match cursor {
            None => {
                if CURRENT_THREAD_IS_CFS && !CURRENT_THREAD_CTX.is_null() {
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

/// Visit scheduler-visible CFS threads without exposing raw traversal cursors.
pub fn traverse_run_queue_fn<F>(mut f: F)
where
    F: FnMut(&ThreadCtx),
{
    critical_section(|| unsafe {
        let mut cursor = traverse_run_queue(None);
        while let Some(thread) = cursor {
            f(&*thread);
            cursor = traverse_run_queue(Some(thread));
        }
    });
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
pub unsafe fn enqueue_thread(thread: *mut ThreadCtx) {
    unsafe {
        (*thread).state = ThreadState::Ready;
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

/// Remove a thread from the scheduler run queue if it is currently queued.
#[allow(dead_code)]
pub unsafe fn dequeue_thread(thread: *mut ThreadCtx) {
    unsafe {
        if (*thread).state == ThreadState::Ready {
            let entity = cfs_sched_entity(thread);
            (*CFS_RUN_QUEUE.get()).remove(entity);
            let priority_sum = (*CFS_RUN_QUEUE.priority_sum()).saturating_sub((*entity).priority);
            *CFS_RUN_QUEUE.priority_sum() = priority_sum;
        }
    }
}

/// Since this is called from dispatch_expired, WAIT_KTIMER is already popped
/// from the KTIMER_QUEUE, so calling program_wait_ktimer() at the end of this function
/// will generate a program panic.
pub unsafe fn enqueue_runq_from_waitq(thread: *mut ThreadCtx) {
    unsafe {
        let entity = cfs_sched_entity(thread);
        let priority_sum = (*CFS_RUN_QUEUE.priority_sum()).saturating_add((*entity).priority);
        let tree = &mut *CFS_RUN_QUEUE.get();
        debug_assert!(
            !tree.contains(entity.cast_const()),
            "sched entity is already queued"
        );

        (*entity).reset_links();
        *CFS_RUN_QUEUE.priority_sum() = priority_sum;
        (*thread).state = ThreadState::Ready;

        // Update cfs_rq with priority_sum
        let mut updated = RBTree::new();

        while let Some(entity) = tree.pop_first() {
            let priority_sum = *CFS_RUN_QUEUE.priority_sum();
            let priority = (*entity).priority;

            (*entity).sched_tick_cnt =
                cfs_sched_ticks_from_vruntime((*entity).vruntime, priority, priority_sum);

            updated.insert(entity);
        }

        *tree = updated;

        update_from_leftmost(entity);
        (*CFS_RUN_QUEUE.get()).insert(entity);
    }
}

pub(crate) fn dequeue_runq_to_waitq(thread: *mut ThreadCtx) -> Result<(), WaitQueueError> {
    critical_section(|| unsafe {
        if thread.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        let entity = cfs_sched_entity(thread);
        // If (*thread).state is Running, it is not in the runq.
        if (*thread).state == ThreadState::Ready {
            (*CFS_RUN_QUEUE.get()).remove(entity);
        }
        (*thread).state = ThreadState::Waiting;

        insert_wait_thread(thread);
        let priority_sum = (*CFS_RUN_QUEUE.priority_sum()).saturating_sub((*entity).priority);
        *CFS_RUN_QUEUE.priority_sum() = priority_sum;
        program_wait_ktimer();

        Ok(())
    })
}

pub fn dequeue_cfs_thread_to_waitq(thread: &mut CfsThread) -> Result<(), WaitQueueError> {
    dequeue_runq_to_waitq(thread.thread_ctx_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ktimer::init_ktimer_queue;
    use crate::thread::{CfsThread, ThreadState};
    use crate::waitq::{WAIT_QUEUE, WaitEntity, wait_entity};
    use std::sync::Mutex;
    use std::vec::Vec;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_run_queue() {
        unsafe {
            init_cfs_rq();
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    fn reset_wait_queue() {
        unsafe {
            *WAIT_QUEUE.get() = RBTree::new();
        }
    }

    fn cfs_thread(name: &'static str, priority: u32, vruntime: u64) -> CfsThread {
        let mut thread = CfsThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 0,
                name,
                state: ThreadState::Ready,
                is_cfs: true,
            },
            wait_entity: WaitEntity::new(),
            sched_entity: SchedEntity::new(priority),
        };
        thread.sched_entity.vruntime = vruntime;
        thread
    }

    fn collect_thread_names() -> Vec<&'static str> {
        let mut names = Vec::new();
        traverse_run_queue_fn(|thread| names.push(thread.name));

        names
    }

    #[test]
    fn sched_entities_order_by_vruntime() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_run_queue();
        let mut first = cfs_thread("first", 1, 30);
        let mut second = cfs_thread("second", 1, 10);
        let mut third = cfs_thread("third", 1, 20);

        unsafe {
            (*CFS_RUN_QUEUE.get()).insert(&mut first.sched_entity);
            (*CFS_RUN_QUEUE.get()).insert(&mut second.sched_entity);
            (*CFS_RUN_QUEUE.get()).insert(&mut third.sched_entity);
        }

        let tree = unsafe { &*CFS_RUN_QUEUE.get() };
        unsafe {
            assert_eq!((*tree.first()).vruntime(), 10);
            assert_eq!((*tree.last()).vruntime(), 30);
        }
    }

    #[test]
    fn enqueue_thread_updates_priority_sum_and_traversal_order() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_run_queue();
        let mut first = cfs_thread("first", 2, 30);
        let mut second = cfs_thread("second", 4, 10);
        let mut third = cfs_thread("third", 8, 20);

        unsafe {
            enqueue_thread(&mut first.thread);
            enqueue_thread(&mut second.thread);
            enqueue_thread(&mut third.thread);
        }

        assert_eq!(unsafe { *CFS_RUN_QUEUE.priority_sum() }, 14);
        assert_eq!(collect_thread_names(), ["first", "second", "third"]);
    }

    #[test]
    fn lower_numeric_priority_accumulates_vruntime_more_slowly() {
        let high_priority_delta = cfs_vruntime_delta(20, 1, 5);
        let low_priority_delta = cfs_vruntime_delta(20, 4, 5);

        assert_eq!(high_priority_delta, 4);
        assert_eq!(low_priority_delta, 16);
        assert!(high_priority_delta < low_priority_delta);
    }

    #[test]
    fn traverse_run_queue_includes_running_cfs_thread_first() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_run_queue();
        let mut running = cfs_thread("running", 1, 0);
        let mut queued = cfs_thread("queued", 1, 10);

        unsafe {
            CURRENT_THREAD_CTX = &mut running.thread;
            CURRENT_THREAD_IS_CFS = true;
            enqueue_thread(&mut queued.thread);
        }

        assert_eq!(collect_thread_names(), ["running", "queued"]);
    }

    #[test]
    fn dequeue_thread_removes_ready_thread_and_saturates_priority_sum() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_run_queue();
        let mut first = cfs_thread("first", 3, 0);
        let mut second = cfs_thread("second", 5, 10);

        unsafe {
            enqueue_thread(&mut first.thread);
            enqueue_thread(&mut second.thread);
            dequeue_thread(&mut first.thread);
        }

        assert_eq!(unsafe { *CFS_RUN_QUEUE.priority_sum() }, 5);
        assert_eq!(collect_thread_names(), ["second"]);
        assert!(!first.sched_entity.is_linked());
    }

    #[test]
    fn dequeue_runq_to_waitq_moves_thread_between_queues() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_run_queue();
        reset_wait_queue();
        unsafe {
            init_ktimer_queue();
        }

        let mut thread = cfs_thread("waiting", 3, 0);

        unsafe {
            enqueue_thread(&mut thread.thread);
            assert!((*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(&mut thread.thread)));

            assert!(dequeue_cfs_thread_to_waitq(&mut thread).is_ok());

            assert!(thread.thread.state == ThreadState::Waiting);
            assert!(!(*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(&mut thread.thread)));
            assert!((*WAIT_QUEUE.get()).contains(wait_entity(&mut thread.thread)));
            assert_eq!(*CFS_RUN_QUEUE.priority_sum(), 0);
        }
    }

    #[test]
    fn update_from_leftmost_aligns_detached_entity() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_run_queue();
        let mut queued = cfs_thread("queued", 4, 40);
        let mut detached = cfs_thread("detached", 2, 0);

        unsafe {
            enqueue_thread(&mut queued.thread);
            update_from_leftmost(&mut detached.sched_entity);
        }

        assert_eq!(detached.sched_entity.vruntime(), 40);
        assert_eq!(detached.sched_entity.sched_tick_cnt(), 80);
    }
}
