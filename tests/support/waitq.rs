extern crate std;

use super::*;
use crate::diagnostics::waitq::{traverse_wait_queue, traverse_wait_queue_fn};
use crate::runq::SchedEntity;
use crate::test_support::TEST_LOCK;
use crate::thread::{CfsThread, ThreadCtx, ThreadHandle, ThreadKind, ThreadState};
use std::vec::Vec;

fn reset_wait_queue() {
    unsafe {
        WAIT_QUEUE.reset_for_test();
    }
}

fn cfs_thread(name: &'static str, wake_at: u64, waitevt: Option<SyncType>) -> CfsThread {
    let mut thread = CfsThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 0,
            name,
            state: if wake_at == u64::MAX {
                ThreadState::Suspended
            } else {
                ThreadState::Waiting
            },
            kind: ThreadKind::Cfs,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: crate::sync::SyncEntity::new(),
        sched_entity: SchedEntity::new(1),
    };
    thread.wait_entity.wake_at = wake_at;
    thread.wait_entity.waitevt = waitevt;
    thread
}

unsafe fn thread_handle(thread: *mut ThreadCtx) -> ThreadHandle {
    unsafe { ThreadHandle::from_thread_ctx(thread) }
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

pub fn wait_entities_order_by_ticks_then_event() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_wait_queue();
    let mut first = cfs_thread("first", 10, Some(SyncType::BinarySemaphore));
    let mut second = cfs_thread("second", 5, Some(SyncType::CountingSemaphore));
    let mut third = cfs_thread("third", 10, Some(SyncType::Mutex));

    unsafe {
        insert_wait_thread(thread_handle(&mut first.thread));
        insert_wait_thread(thread_handle(&mut second.thread));
        insert_wait_thread(thread_handle(&mut third.thread));
    }

    assert_eq!(collect_wake_at(), [5, 10, 10]);
    let mut names = Vec::new();
    traverse_wait_queue_fn(|thread| names.push(thread.thread_ctx().name));
    assert_eq!(names, ["second", "third", "first"]);
    let first_thread = unsafe { traverse_wait_queue(None).unwrap() };
    let second_thread = unsafe { traverse_wait_queue(Some(first_thread)).unwrap() };
    let third_thread = unsafe { traverse_wait_queue(Some(second_thread)).unwrap() };

    unsafe {
        assert_eq!((*first_thread.as_ptr()).name, "second");
        assert_eq!((*second_thread.as_ptr()).name, "third");
        assert_eq!((*third_thread.as_ptr()).name, "first");
        assert!(traverse_wait_queue(Some(third_thread)).is_none());
    }
}

pub fn remaining_time_uses_absolute_wake_deadline() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_wait_queue();
    let mut first = cfs_thread("first", 3, None);
    let mut second = cfs_thread("second", 12, None);
    let mut third = cfs_thread("third", 7, None);

    unsafe {
        insert_wait_thread(thread_handle(&mut first.thread));
        insert_wait_thread(thread_handle(&mut second.thread));
        insert_wait_thread(thread_handle(&mut third.thread));
    }

    assert_eq!(first.wait_entity.wake_at, 3);
    assert_eq!(second.wait_entity.wake_at, 12);
    assert_eq!(third.wait_entity.wake_at, 7);
    assert_eq!(collect_wake_at(), [3, 7, 12]);
    assert_eq!(collect_remaining(5), [0, 2, 7]);
}

pub fn pop_expired_wait_thread_only_pops_zero_tick_threads() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_wait_queue();
    let mut expired = cfs_thread("expired", 0, None);
    let mut pending = cfs_thread("pending", 4, None);

    unsafe {
        insert_wait_thread(thread_handle(&mut pending.thread));
        insert_wait_thread(thread_handle(&mut expired.thread));
    }

    let popped = unsafe { pop_expired_wait_thread(0) };
    assert!(ptr::eq(
        popped.expect("expired thread should be popped").as_ptr(),
        &expired.thread
    ));
    assert_eq!(unsafe { pop_expired_wait_thread(0) }, None);
    assert_eq!(collect_wake_at(), [4]);
}

pub fn remove_wait_thread_detaches_entity() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_wait_queue();
    let mut first = cfs_thread("first", 1, None);
    let mut second = cfs_thread("second", 2, None);

    unsafe {
        insert_wait_thread(thread_handle(&mut first.thread));
        insert_wait_thread(thread_handle(&mut second.thread));
        remove_wait_thread(thread_handle(&mut first.thread));
    }

    assert_eq!(collect_wake_at(), [2]);
    assert!(!first.wait_entity.rb_node.is_linked());
}

pub fn indefinite_waits_stay_visible_after_finite_waits_and_never_expire() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_wait_queue();
    let mut first = cfs_thread("first", u64::MAX, Some(SyncType::Mutex));
    let mut finite = cfs_thread("finite", 10, None);
    let mut rt = crate::test_support::rt_thread("rt");
    rt.wait_entity.wake_at = u64::MAX;

    unsafe {
        let first_handle = thread_handle(ptr::addr_of_mut!(first).cast::<ThreadCtx>());
        let finite_handle = thread_handle(ptr::addr_of_mut!(finite).cast::<ThreadCtx>());
        let rt_handle = thread_handle(ptr::addr_of_mut!(rt).cast::<ThreadCtx>());
        insert_wait_thread(first_handle);
        insert_wait_thread(finite_handle);
        insert_wait_thread(rt_handle);
        assert_eq!(rt.thread.state, ThreadState::Suspended);
        assert_eq!(collect_wake_at(), [10]);
        let mut names = Vec::new();
        traverse_wait_queue_fn(|thread| names.push(thread.thread_ctx().name));
        assert_eq!(names, ["finite", "first", "rt"]);
        assert_eq!(pop_expired_wait_thread(9), None);
        assert_eq!(pop_expired_wait_thread(10), Some(finite_handle));
        assert_eq!(pop_expired_wait_thread(u64::MAX), None);
        remove_wait_thread(first_handle);
        remove_wait_thread(rt_handle);
        assert!(traverse_wait_queue(None).is_none());
    }
}

pub fn suspended_list_handles_middle_head_tail_and_reinsertion() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_wait_queue();
    let mut first = cfs_thread("first", u64::MAX, None);
    let mut middle = cfs_thread("middle", u64::MAX, None);
    let mut last = cfs_thread("last", u64::MAX, None);

    unsafe {
        let first_handle = thread_handle(ptr::addr_of_mut!(first).cast::<ThreadCtx>());
        let middle_handle = thread_handle(ptr::addr_of_mut!(middle).cast::<ThreadCtx>());
        let last_handle = thread_handle(ptr::addr_of_mut!(last).cast::<ThreadCtx>());
        insert_wait_thread(first_handle);
        insert_wait_thread(middle_handle);
        insert_wait_thread(last_handle);
        remove_wait_thread(middle_handle);
        insert_wait_thread(middle_handle);
        let mut names = Vec::new();
        traverse_wait_queue_fn(|thread| names.push(thread.thread_ctx().name));
        assert_eq!(names, ["first", "last", "middle"]);
        remove_wait_thread(first_handle);
        remove_wait_thread(middle_handle);
        assert_eq!(traverse_wait_queue(None), Some(last_handle));
        assert!(traverse_wait_queue(Some(last_handle)).is_none());
        remove_wait_thread(last_handle);
        assert!((*WAIT_QUEUE.suspended.get()).head.is_null());
        assert!((*WAIT_QUEUE.suspended.get()).tail.is_null());
        // A detached wait entity can subsequently register a finite timeout.
        (*last_handle.as_ptr()).set_state(ThreadState::Ready);
        (*wait_entity(last_handle)).wake_at = 5;
        insert_wait_thread(last_handle);
        assert_eq!(last.thread.state, ThreadState::Waiting);
        assert_eq!(pop_expired_wait_thread(5), Some(last_handle));
        assert!(traverse_wait_queue(None).is_none());
    }
}
