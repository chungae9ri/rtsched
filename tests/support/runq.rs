extern crate std;

use super::*;
use crate::ktimer::init_ktimer_queue;
use crate::test_support::TEST_LOCK;
use crate::thread::{CfsThread, ThreadCtx, ThreadHandle, ThreadKind, ThreadState};
use crate::waitq::{WAIT_QUEUE, WaitEntity, wait_entity};
use std::vec::Vec;

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
            kind: ThreadKind::Cfs,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: crate::sync::SyncEntity::new(),
        sched_entity: SchedEntity::new(priority),
    };
    thread.sched_entity.vruntime = vruntime;
    thread
}

unsafe fn thread_handle(thread: *mut ThreadCtx) -> ThreadHandle {
    unsafe { ThreadHandle::from_thread_ctx(thread) }
}

fn collect_thread_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    traverse_run_queue_fn(|thread| names.push(thread.thread_ctx().name));

    names
}

pub fn sched_entities_order_by_vruntime() {
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

pub fn enqueue_thread_updates_priority_sum_and_traversal_order() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_run_queue();
    let mut first = cfs_thread("first", 2, 30);
    let mut second = cfs_thread("second", 4, 10);
    let mut third = cfs_thread("third", 8, 20);

    unsafe {
        enqueue_thread(thread_handle(&mut first.thread));
        enqueue_thread(thread_handle(&mut second.thread));
        enqueue_thread(thread_handle(&mut third.thread));
    }

    assert_eq!(unsafe { *CFS_RUN_QUEUE.priority_sum() }, 14);
    assert_eq!(collect_thread_names(), ["first", "second", "third"]);
}

pub fn lower_numeric_priority_accumulates_vruntime_more_slowly() {
    let high_priority_delta = cfs_vruntime_delta(20, 1, 5);
    let low_priority_delta = cfs_vruntime_delta(20, 4, 5);

    assert_eq!(high_priority_delta, 4);
    assert_eq!(low_priority_delta, 16);
    assert!(high_priority_delta < low_priority_delta);
}

pub fn traverse_run_queue_includes_running_cfs_thread_first() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_run_queue();
    let mut running = cfs_thread("running", 1, 0);
    let mut queued = cfs_thread("queued", 1, 10);

    unsafe {
        CURRENT_THREAD_CTX = &mut running.thread;
        CURRENT_THREAD_IS_CFS = true;
        enqueue_thread(thread_handle(&mut queued.thread));
    }

    assert_eq!(collect_thread_names(), ["running", "queued"]);
}

pub fn dequeue_thread_removes_ready_thread_and_saturates_priority_sum() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_run_queue();
    let mut first = cfs_thread("first", 3, 0);
    let mut second = cfs_thread("second", 5, 10);

    unsafe {
        enqueue_thread(thread_handle(&mut first.thread));
        enqueue_thread(thread_handle(&mut second.thread));
        dequeue_thread(thread_handle(&mut first.thread));
    }

    assert_eq!(unsafe { *CFS_RUN_QUEUE.priority_sum() }, 5);
    assert_eq!(collect_thread_names(), ["second"]);
    assert!(!first.sched_entity.is_linked());
}

pub fn dequeue_runq_to_waitq_moves_thread_between_queues() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_run_queue();
    reset_wait_queue();
    unsafe {
        init_ktimer_queue();
    }

    let mut thread = cfs_thread("waiting", 3, 0);

    unsafe {
        let handle = thread_handle(&mut thread.thread);
        enqueue_thread(handle);
        assert!((*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(handle)));

        assert!(dequeue_cfs_thread_to_waitq(&mut thread).is_ok());

        assert!(thread.thread.state == ThreadState::Waiting);
        assert!(!(*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(handle)));
        assert!((*WAIT_QUEUE.get()).contains(wait_entity(handle)));
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), 0);
    }
}

pub fn update_from_leftmost_aligns_detached_entity() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_run_queue();
    let mut queued = cfs_thread("queued", 4, 40);
    let mut detached = cfs_thread("detached", 2, 0);

    unsafe {
        enqueue_thread(thread_handle(&mut queued.thread));
        update_from_leftmost(&mut detached.sched_entity);
    }

    assert_eq!(detached.sched_entity.vruntime(), 40);
    assert_eq!(detached.sched_entity.sched_tick_cnt(), 80);
}
