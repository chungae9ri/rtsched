extern crate std;

use super::*;
use crate::test_support::TEST_LOCK;
use crate::thread::{RtThread, ThreadCtx, ThreadHandle, ThreadKind, ThreadState};
use crate::waitq::{WAIT_QUEUE, WaitEntity, insert_wait_thread, wait_entity};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::string::String;
use std::sync::Mutex;
use std::vec::Vec;

static DEADLINE_MISS_PRINTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn capture_deadline_miss_print(message: &str) {
    DEADLINE_MISS_PRINTED
        .lock()
        .unwrap()
        .push(String::from(message));
}

fn discard_print(_message: &str) {}

fn rt_thread(name: &'static str) -> RtThread {
    RtThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 1,
            name,
            state: ThreadState::Ready,
            kind: ThreadKind::Rt,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: crate::sync::SyncEntity::new(),
        ktimer_entity: ptr::null_mut(),
        runtime: 0,
    }
}

unsafe fn thread_handle(thread: *mut ThreadCtx) -> ThreadHandle {
    unsafe { ThreadHandle::from_thread_ctx(thread) }
}

fn reset_wait_queue() {
    unsafe {
        *WAIT_QUEUE.get() = RBTree::new();
    }
}

unsafe fn reset_global_ktimer_queue() {
    unsafe {
        ptr::write(KTIMER_QUEUE.get(), KTimerQueue::new());
        ptr::write(&raw mut NEXT_KTIMER, ptr::null_mut());
    }
}

fn collect_deadlines_at(queue: &KTimerQueue) -> Vec<u64> {
    let mut deadlines = Vec::new();
    let mut entity = queue.first();

    while !entity.is_null() {
        unsafe {
            deadlines.push((*entity).expire_at());
        }
        entity = queue.next(entity);
    }

    deadlines
}

fn collect_remaining(queue: &KTimerQueue) -> Vec<u32> {
    let mut remaining = Vec::new();
    let mut entity = queue.first();

    while !entity.is_null() {
        unsafe {
            remaining.push((*entity).remaining_at(queue.now_ticks()));
        }
        entity = queue.next(entity);
    }

    remaining
}

fn collect_active_deadlines_at(queue: &KTimerQueue) -> Vec<u64> {
    let mut deadlines = Vec::new();
    let mut entity = queue.first();

    while !entity.is_null() {
        unsafe {
            if (*entity).is_active() {
                deadlines.push((*entity).expire_at());
            }
        }
        entity = queue.next(entity);
    }

    deadlines
}

pub fn reload_from_ticks_converts_to_scheduler_timer_reload() {
    assert_eq!(reload_from_ticks(0), None);
    assert_eq!(reload_from_ticks(1), Some(0));
    assert_eq!(reload_from_ticks(2), Some(SCHEDULER_TIMER_RELOAD_MIN));
    assert_eq!(reload_from_ticks(42), Some(41));
    assert_eq!(
        reload_from_ticks(SCHEDULER_TIMER_RELOAD_MAX + 1),
        Some(SCHEDULER_TIMER_RELOAD_MAX)
    );
    assert_eq!(reload_from_ticks(SCHEDULER_TIMER_RELOAD_MAX + 2), None);
}

pub fn writable_reload_clamps_raw_zero_to_minimum_reload() {
    assert_eq!(writable_reload(0), SCHEDULER_TIMER_RELOAD_MIN);
    assert_eq!(
        writable_reload(SCHEDULER_TIMER_RELOAD_MAX + 1),
        SCHEDULER_TIMER_RELOAD_MAX
    );
    assert_eq!(
        programmable_reload_from_ticks(0),
        SCHEDULER_TIMER_RELOAD_MIN
    );
    assert_eq!(
        programmable_reload_from_ticks(1),
        SCHEDULER_TIMER_RELOAD_MIN
    );
}

pub fn insert_orders_timers_by_deadline() {
    let mut queue = KTimerQueue::new();
    let mut timers = [
        KTimerEntity::new(30),
        KTimerEntity::new(10),
        KTimerEntity::new(20),
        KTimerEntity::new(5),
    ];

    for timer in &mut timers {
        unsafe {
            queue.insert(timer);
        }
    }

    assert_eq!(queue.len(), timers.len());
    assert_eq!(collect_deadlines_at(&queue), [5, 10, 20, 30]);
    assert_eq!(collect_remaining(&queue), [5, 10, 20, 30]);
    assert_eq!(queue.next_deadline(), Some(5));
    assert_eq!(queue.next_reload(), Some(4));
    unsafe {
        assert_eq!((*queue.first()).expire_at(), 5);
        assert_eq!((*queue.last()).expire_at(), 30);
    }
}

pub fn equal_deadlines_keep_strict_entity_ordering() {
    let mut queue = KTimerQueue::new();
    let mut first = KTimerEntity::new(12);
    let mut second = KTimerEntity::new(12);

    unsafe {
        queue.insert(&mut first);
        queue.insert(&mut second);
    }

    assert_eq!(queue.len(), 2);
    assert_eq!(collect_deadlines_at(&queue), [12, 12]);
}

pub fn ktimer_queue_rejects_duplicate_timer_insertions() {
    let mut queue = KTimerQueue::new();
    let mut timer = KTimerEntity::new(12);

    unsafe {
        queue.insert(&mut timer);
        queue.insert(&mut timer);
    }
}

pub fn advance_time_updates_queue_clock_without_rewriting_deadlines() {
    let mut queue = KTimerQueue::new();
    let mut short = KTimerEntity::new(3);
    let mut long = KTimerEntity::new(20);
    let mut parked = WaitKTimer::new();

    unsafe {
        queue.insert(&mut short);
        queue.insert(&mut long);
        queue.insert(parked.entity_mut());
    }
    queue.advance_time(5);

    assert_eq!(queue.now_ticks(), 5);
    assert_eq!(short.expire_at(), 3);
    assert_eq!(long.expire_at(), 20);
    assert_eq!(parked.entity.expire_at(), KTIMER_EXPIRE_NEVER);
    assert_eq!(short.remaining_at(queue.now_ticks()), 0);
    assert_eq!(long.remaining_at(queue.now_ticks()), 15);
    assert_eq!(
        parked.entity.remaining_at(queue.now_ticks()),
        SCHEDULER_TIMER_RELOAD_MAX
    );
    assert_eq!(
        collect_remaining(&queue),
        [0, 15, SCHEDULER_TIMER_RELOAD_MAX]
    );
}

pub fn next_reload_clamps_immediate_deadlines_to_minimum_reload() {
    let mut queue = KTimerQueue::new();
    let mut expired = KTimerEntity::new(0);

    unsafe {
        queue.insert(&mut expired);
    }

    assert_eq!(queue.next_deadline(), Some(0));
    assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MIN));
}

pub fn next_reload_clamps_far_deadlines_to_maximum_reload() {
    let mut queue = KTimerQueue::new();
    let mut far = KTimerEntity::new(1);
    far.set_expire_at(u64::from(SCHEDULER_TIMER_RELOAD_MAX) + 42);

    unsafe {
        queue.insert(&mut far);
    }

    assert_eq!(queue.next_deadline(), Some(SCHEDULER_TIMER_RELOAD_MAX));
    assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MAX));
}

pub fn cfs_reload_honors_boosted_deadline_before_execution_slice() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        init_ktimer_queue();
        crate::sched::init_cfs(100, 25);

        let queue = &mut *KTIMER_QUEUE.get();
        let cfs = ptr::addr_of_mut!(CFS_KTIMER.entity);

        queue.remove(cfs);
        (*cfs).set_expire_at(5);
        (*cfs).reset_links();
        queue.insert(cfs);

        assert_eq!(queue.next_reload(), Some(4));
    }
}

pub fn long_rt_deadline_is_dispatched_after_multiple_scheduler_timer_chunks() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");
    let max_chunk_ticks = SCHEDULER_TIMER_RELOAD_MAX + 1;
    let long_deadline = u64::from(max_chunk_ticks) * 2 + 5;

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        ktimer.entity.set_expire_at(long_deadline);
        queue.insert(ktimer.entity_mut());
    }

    assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MAX));

    queue.advance_time(max_chunk_ticks);
    let next = unsafe { queue.dispatch_expired(max_chunk_ticks) };
    assert!(ptr::eq(next, ktimer.entity_mut()));
    assert_eq!(queue.now_ticks(), u64::from(max_chunk_ticks));
    assert_eq!(ktimer.entity.expire_at(), long_deadline);
    assert_eq!(ktimer.miss_cnt, 0);
    assert_eq!(queue.next_reload(), Some(SCHEDULER_TIMER_RELOAD_MAX));

    queue.advance_time(max_chunk_ticks);
    let next = unsafe { queue.dispatch_expired(max_chunk_ticks) };
    assert!(ptr::eq(next, ktimer.entity_mut()));
    assert_eq!(queue.now_ticks(), u64::from(max_chunk_ticks) * 2);
    assert_eq!(ktimer.entity.expire_at(), long_deadline);
    assert_eq!(ktimer.miss_cnt, 0);
    assert_eq!(queue.next_reload(), Some(4));

    queue.advance_time(5);
    let next = unsafe { queue.dispatch_expired(5) };

    assert!(ptr::eq(next, ktimer.entity_mut()));
    assert_eq!(queue.now_ticks(), long_deadline);
    assert_eq!(ktimer.miss_cnt, 0);
    assert_eq!(ktimer.entity.expire_at(), long_deadline + 50);
}

pub fn first_active_skips_inactive_timers() {
    let mut queue = KTimerQueue::new();
    let mut inactive_early = KTimerEntity::new(5);
    let mut active_late = KTimerEntity::new(20);
    let mut inactive_middle = KTimerEntity::new(10);

    inactive_early.set_active(false);
    inactive_middle.set_active(false);

    unsafe {
        queue.insert(&mut active_late);
        queue.insert(&mut inactive_early);
        queue.insert(&mut inactive_middle);
    }

    assert!(ptr::eq(queue.first_active(), &active_late));
}

pub fn active_timers_remain_sorted_by_absolute_deadline() {
    let mut queue = KTimerQueue::new();
    let mut active_middle = KTimerEntity::new(30);
    let mut inactive_first = KTimerEntity::new(5);
    let mut active_first = KTimerEntity::new(10);
    let mut inactive_late = KTimerEntity::new(40);
    let mut active_last = KTimerEntity::new(50);

    inactive_first.set_active(false);
    inactive_late.set_active(false);

    unsafe {
        queue.insert(&mut active_middle);
        queue.insert(&mut inactive_first);
        queue.insert(&mut active_first);
        queue.insert(&mut inactive_late);
        queue.insert(&mut active_last);
    }

    assert_eq!(collect_deadlines_at(&queue), [5, 10, 30, 40, 50]);
    assert_eq!(collect_active_deadlines_at(&queue), [10, 30, 50]);
    assert!(ptr::eq(queue.first_active(), &active_first));
}

pub fn remove_detaches_timer_from_queue() {
    let mut queue = KTimerQueue::new();
    let mut timers = [
        KTimerEntity::new(8),
        KTimerEntity::new(4),
        KTimerEntity::new(12),
    ];

    for timer in &mut timers {
        unsafe {
            queue.insert(timer);
        }
    }

    let removed = unsafe { queue.remove(&mut timers[1]) };

    assert!(ptr::eq(removed, &timers[1]));
    assert!(!timers[1].is_linked());
    assert_eq!(queue.len(), 2);
    assert_eq!(collect_deadlines_at(&queue), [8, 12]);
}

pub fn left_most_cache_tracks_queue_mutations() {
    let mut queue = KTimerQueue::new();
    let mut late = KTimerEntity::new(30);
    let mut early = KTimerEntity::new(5);
    let mut middle = KTimerEntity::new(20);

    assert!(queue.left_most.is_null());
    assert!(queue.first_active.is_null());

    unsafe {
        queue.insert(&mut late);
    }
    assert!(ptr::eq(queue.first(), &late));
    assert!(ptr::eq(queue.first_active(), &late));

    unsafe {
        queue.insert(&mut early);
    }
    assert!(ptr::eq(queue.first(), &early));
    assert!(ptr::eq(queue.first_active(), &early));

    unsafe {
        queue.insert(&mut middle);
    }
    assert!(ptr::eq(queue.first(), &early));
    assert!(ptr::eq(queue.first_active(), &early));

    unsafe {
        queue.remove(&mut early);
    }
    assert!(ptr::eq(queue.first(), &middle));
    assert!(ptr::eq(queue.first_active(), &middle));

    let popped = unsafe { queue.pop_first() }.unwrap() as *mut KTimerEntity;
    assert!(ptr::eq(popped, ptr::addr_of_mut!(middle)));
    assert!(ptr::eq(queue.first(), &late));
    assert!(ptr::eq(queue.first_active(), &late));

    unsafe {
        queue.remove(&mut late);
    }
    assert!(queue.left_most.is_null());
    assert!(queue.first_active.is_null());
    assert!(queue.first().is_null());
    assert!(queue.first_active().is_null());
}

pub fn first_active_cache_tracks_inactive_frontier_mutations() {
    let mut queue = KTimerQueue::new();
    let mut inactive_early = KTimerEntity::new(5);
    let mut active_middle = KTimerEntity::new(10);
    let mut inactive_late = KTimerEntity::new(20);
    let mut active_late = KTimerEntity::new(30);

    inactive_early.set_active(false);
    inactive_late.set_active(false);

    unsafe {
        queue.insert(&mut inactive_early);
    }
    assert!(ptr::eq(queue.first(), &inactive_early));
    assert!(queue.first_active().is_null());

    unsafe {
        queue.insert(&mut active_late);
    }
    assert!(ptr::eq(queue.first_active(), &active_late));

    unsafe {
        queue.insert(&mut active_middle);
    }
    assert!(ptr::eq(queue.first_active(), &active_middle));

    unsafe {
        queue.insert(&mut inactive_late);
    }
    assert!(ptr::eq(queue.first_active(), &active_middle));

    unsafe {
        queue.remove(&mut active_middle);
    }
    assert!(ptr::eq(queue.first_active(), &active_late));

    unsafe {
        queue.remove(&mut active_late);
    }
    assert!(queue.first_active().is_null());
    assert!(ptr::eq(queue.first(), &inactive_early));
}

pub fn refresh_next_ktimer_leaves_inactive_cfs_timer_inactive() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        init_ktimer_queue();
        crate::sched::init_cfs(100, 25);

        let queue = &mut *KTIMER_QUEUE.get();
        let cfs = ptr::addr_of_mut!(CFS_KTIMER.entity);
        queue.remove(cfs);
        (*cfs).set_active(false);
        (*cfs).reset_links();
        queue.insert(cfs);

        refresh_next_ktimer(queue);

        assert!(next_ktimer().is_null());
        assert!(!(*cfs).is_active());
        assert!(queue.first_active().is_null());
        assert!(queue.contains(cfs.cast_const()));
    }
}

pub fn dispatch_expired_returns_null_when_no_timer_is_active() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        init_ktimer_queue();
        crate::sched::init_cfs(100, 25);

        let queue = &mut *KTIMER_QUEUE.get();
        let cfs = ptr::addr_of_mut!(CFS_KTIMER.entity);
        queue.remove(cfs);
        (*cfs).set_expire_at(100);
        (*cfs).set_active(false);
        (*cfs).reset_links();
        queue.insert(cfs);

        let next = queue.dispatch_expired(0);

        assert!(next.is_null());
        assert!(!(*cfs).is_active());
        assert!(queue.first_active().is_null());
    }
}

pub fn pop_first_returns_timers_in_deadline_order() {
    let mut queue = KTimerQueue::new();
    let mut timers = [
        KTimerEntity::new(40),
        KTimerEntity::new(15),
        KTimerEntity::new(25),
        KTimerEntity::new(1),
    ];

    for timer in &mut timers {
        unsafe {
            queue.insert(timer);
        }
    }

    let mut popped = Vec::new();
    while let Some(timer) = unsafe { queue.pop_first() } {
        popped.push(timer.expire_at());
        assert!(!timer.is_linked());
    }

    assert_eq!(popped, [1, 15, 25, 40]);
    assert!(queue.is_empty());
    assert_eq!(queue.len(), 0);
    assert_eq!(queue.next_deadline(), None);
    assert_eq!(queue.next_reload(), None);
}

pub fn rt_timing_constructor_separates_period_deadline_and_budget() {
    let ktimer = RtKTimer::new_with_timing(RtTiming::new(100, 40, 20), ptr::null_mut(), "rt");

    assert_eq!(ktimer.period_ticks(), 100);
    assert_eq!(ktimer.relative_deadline_ticks(), 40);
    assert_eq!(ktimer.budget_ticks(), 20);
    assert_eq!(ktimer.timing(), RtTiming::new(100, 40, 20));
    assert_eq!(ktimer.entity.timing(), RtTiming::new(100, 40, 20));
    assert_eq!(ktimer.entity.expire_at(), 40);
}

pub fn rt_yield_marks_timer_inactive_and_preserves_remaining_period() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(100, ptr::null_mut(), "rt");
    let mut active_later = KTimerEntity::new(200);

    unsafe {
        reset_global_ktimer_queue();
        ktimer.init_rt_ktimer(&mut rt.thread);
        {
            let queue = &mut *KTIMER_QUEUE.get();
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);
        }

        let next = yield_ktimer(ktimer.entity_mut(), 20, false);
        assert!(ptr::eq(next, &active_later));
    }

    assert_eq!(rt.runtime, 20);
    assert!(!ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 100);
    unsafe {
        let queue = &*KTIMER_QUEUE.get();
        assert_eq!(queue.now_ticks(), 20);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 80);
    }
}

pub fn rt_yield_with_reset_runtime_finishes_current_job_window() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(60, ptr::null_mut(), "rt");
    let mut active_later = KTimerEntity::new(120);

    unsafe {
        reset_global_ktimer_queue();
        ktimer.init_rt_ktimer(&mut rt.thread);
        {
            let queue = &mut *KTIMER_QUEUE.get();
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);
        }

        let next = yield_ktimer(ktimer.entity_mut(), 15, true);
        assert!(ptr::eq(next, &active_later));
    }

    assert_eq!(rt.runtime, 0);
    assert!(!ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 60);
    unsafe {
        let queue = &*KTIMER_QUEUE.get();
        assert_eq!(queue.now_ticks(), 15);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 45);
    }
}

pub fn rt_yield_without_active_timers_returns_null() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(60, ptr::null_mut(), "rt");

    unsafe {
        reset_global_ktimer_queue();
        CFS_KTIMER = CfsKTimer::new(100, 25, "cfs");
        let cfs = ptr::addr_of_mut!(CFS_KTIMER.entity);
        (*cfs).set_active(false);
        {
            let queue = &mut *KTIMER_QUEUE.get();
            queue.insert(cfs);

            ktimer.init_rt_ktimer(&mut rt.thread);
            queue.insert(ktimer.entity_mut());
        }

        let next = yield_ktimer(ktimer.entity_mut(), 15, true);

        assert!(next.is_null());
        assert!(!(*cfs).is_active());
        assert!((*KTIMER_QUEUE.get()).contains(cfs.cast_const()));
    }

    assert_eq!(rt.runtime, 0);
    assert!(!ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 60);
    unsafe {
        assert_eq!((*KTIMER_QUEUE.get()).now_ticks(), 15);
    }
}

pub fn rt_yield_uses_period_for_next_release_not_relative_deadline() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new_with_timing(RtTiming::new(100, 40, 100), ptr::null_mut(), "rt");
    let mut active_later = KTimerEntity::new(200);

    unsafe {
        reset_global_ktimer_queue();
        ktimer.init_rt_ktimer(&mut rt.thread);
        {
            let queue = &mut *KTIMER_QUEUE.get();
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);
        }

        let next = yield_ktimer(ktimer.entity_mut(), 15, false);
        assert!(ptr::eq(next, &active_later));
    }

    assert_eq!(rt.runtime, 15);
    assert!(!ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 100);
    unsafe {
        let queue = &*KTIMER_QUEUE.get();
        assert_eq!(queue.now_ticks(), 15);
        assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 85);
    }
    assert_eq!(ktimer.miss_cnt, 0);
}

pub fn rt_yield_records_budget_overrun_independent_of_deadline() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new_with_timing(RtTiming::new(100, 80, 10), ptr::null_mut(), "rt");
    let mut active_later = KTimerEntity::new(200);

    unsafe {
        reset_global_ktimer_queue();
        ktimer.init_rt_ktimer(&mut rt.thread);
        {
            let queue = &mut *KTIMER_QUEUE.get();
            queue.insert(ktimer.entity_mut());
            queue.insert(&mut active_later);
        }

        let next = yield_ktimer(ktimer.entity_mut(), 15, false);
        assert!(ptr::eq(next, &active_later));
    }

    assert_eq!(rt.runtime, 15);
    assert_eq!(ktimer.miss_cnt, 1);
    assert!(!ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 100);
}

pub fn wake_wait_thread_charges_wait_elapsed_to_rt_deadline() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_wait_queue();
    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(100, ptr::null_mut(), "rt");

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        rt.runtime = 20;
        rt.thread.state = ThreadState::Waiting;
        rt.wait_entity.set_wake_after(0, 50);
        insert_wait_thread(thread_handle(&mut rt.thread));
    }

    queue.advance_time(50);

    unsafe {
        wake_wait_thread(&mut queue, 30);
    }

    assert!(rt.thread.state == ThreadState::Ready);
    assert_eq!(rt.runtime, 50);
    assert!(ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 100);
    assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 50);
    assert!(ptr::eq(queue.first_active(), ktimer.entity_mut()));
}

pub fn wake_wait_thread_uses_relative_deadline_not_period() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_wait_queue();
    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new_with_timing(RtTiming::new(100, 60, 100), ptr::null_mut(), "rt");

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        rt.runtime = 20;
        rt.thread.state = ThreadState::Waiting;
        rt.wait_entity.set_wake_after(0, 50);
        insert_wait_thread(thread_handle(&mut rt.thread));
    }

    queue.advance_time(50);

    unsafe {
        wake_wait_thread(&mut queue, 30);
    }

    assert_eq!(rt.runtime, 50);
    assert!(ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 60);
    assert_eq!(ktimer.entity.remaining_at(queue.now_ticks()), 10);
}

pub fn rt_only_initialization_does_not_enqueue_cfs_ktimer() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut rt = rt_thread("control");
    let mut ktimer = RtKTimer::new(100, ptr::null_mut(), "control");

    unsafe {
        init_ktimer_queue();
        ktimer.init_rt_ktimer(&mut rt.thread);
        enqueue_ktimer(ktimer.entity_mut());

        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();
        let mut saw_rt_timer = false;
        let mut saw_wait_timer = false;

        while !entity.is_null() {
            assert!(
                !is_cfs_ktimer(entity),
                "RT-only initialization must not enqueue CFS_KTIMER"
            );
            saw_rt_timer |= ptr::eq(entity, ktimer.entity_mut());
            saw_wait_timer |= is_wait_ktimer(entity);
            entity = queue.next(entity);
        }

        assert!(saw_rt_timer);
        assert!(saw_wait_timer);
    }
}

pub fn rt_waiting_thread_is_moved_from_ktimer_queue_to_wait_queue() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_wait_queue();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(100, ptr::null_mut(), "rt");
    rt.wait_entity.set_wake_after(0, 10);

    unsafe {
        init_ktimer_queue();
        crate::sched::init_cfs(1_000, 25);
        ktimer.init_rt_ktimer(&mut rt.thread);
        enqueue_ktimer(ktimer.entity_mut());

        assert!((*KTIMER_QUEUE.get()).contains(ktimer.entity_mut()));

        let handle = thread_handle(&mut rt.thread);

        assert!(dequeue_ktimerq_to_waitq(handle).is_ok());

        assert!(rt.thread.state == ThreadState::Waiting);
        assert!(!(*KTIMER_QUEUE.get()).contains(ktimer.entity_mut()));
        assert!((*WAIT_QUEUE.get()).contains(wait_entity(handle)));
        assert!(ptr::eq(rt_ktimer_entity(handle), ktimer.entity_mut()));

        assert!(enqueue_ktimerq_from_waitq(handle).is_ok());

        assert!(rt.thread.state == ThreadState::Ready);
        assert!(
            (*KTIMER_QUEUE.get()).contains(ktimer.entity_mut()),
            "deadlines after enqueue from waitq: {:?}",
            collect_deadlines_at(&*KTIMER_QUEUE.get())
        );
        assert!(!(*WAIT_QUEUE.get()).contains(wait_entity(handle)));
    }
}

pub fn dispatch_expired_active_rt_timer_records_deadline_miss() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        rt.runtime = 55;
        ktimer.entity.set_expire_at(10);
        queue.insert(ktimer.entity_mut());
    }

    queue.advance_time(10);
    DEADLINE_MISS_PRINTED.lock().unwrap().clear();
    crate::set_print_fn(capture_deadline_miss_print);
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        queue.dispatch_expired(10);
    }));
    crate::set_print_fn(discard_print);

    assert!(result.is_err());
    let printed = DEADLINE_MISS_PRINTED.lock().unwrap();
    assert!(
        printed
            .iter()
            .any(|message| message.contains("ktimer queue statistics"))
    );
    assert!(
        printed
            .iter()
            .any(|message| message.contains("thread statistics"))
    );
    assert_eq!(ktimer.miss_cnt, 1);
    assert_eq!(rt.runtime, 55);
    assert!(ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 10);
}

pub fn dispatch_expired_active_rt_timer_uses_relative_deadline() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new_with_timing(RtTiming::new(100, 40, 20), ptr::null_mut(), "rt");

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        rt.runtime = 45;
        ktimer.entity.set_expire_at(100);
        queue.insert(ktimer.entity_mut());
    }

    queue.advance_time(100);
    let result = catch_unwind(AssertUnwindSafe(|| unsafe {
        queue.dispatch_expired(100);
    }));

    assert!(result.is_err());
    assert_eq!(ktimer.miss_cnt, 1);
    assert_eq!(rt.runtime, 45);
    assert!(ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 100);
}

pub fn dispatch_expired_active_rt_timer_without_runtime_over_deadline_is_not_miss() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        rt.runtime = 50;
        ktimer.entity.set_expire_at(10);
        queue.insert(ktimer.entity_mut());
    }

    queue.advance_time(10);
    let next = unsafe { queue.dispatch_expired(10) };

    assert!(ptr::eq(next, ktimer.entity_mut()));
    assert_eq!(ktimer.miss_cnt, 0);
    assert_eq!(rt.runtime, 0);
    assert!(ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 60);
}

pub fn dispatch_expired_inactive_rt_timer_reactivates_without_miss() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        rt.runtime = 30;
        ktimer.entity.set_active(false);
        ktimer.entity.set_expire_at(10);
        queue.insert(ktimer.entity_mut());
    }

    queue.advance_time(10);
    let next = unsafe { queue.dispatch_expired(10) };

    assert!(ptr::eq(next, ktimer.entity_mut()));
    assert_eq!(ktimer.miss_cnt, 0);
    assert_eq!(rt.runtime, 0);
    assert!(ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 60);
}

pub fn dispatch_expired_inactive_rt_timer_uses_relative_deadline() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut queue = KTimerQueue::new();
    let mut rt = rt_thread("rt");
    let mut ktimer = RtKTimer::new_with_timing(RtTiming::new(100, 40, 20), ptr::null_mut(), "rt");

    unsafe {
        ktimer.init_rt_ktimer(&mut rt.thread);
        rt.runtime = 30;
        ktimer.entity.set_active(false);
        ktimer.entity.set_expire_at(100);
        queue.insert(ktimer.entity_mut());
    }

    queue.advance_time(100);
    let next = unsafe { queue.dispatch_expired(100) };

    assert!(ptr::eq(next, ktimer.entity_mut()));
    assert_eq!(ktimer.miss_cnt, 0);
    assert_eq!(rt.runtime, 0);
    assert!(ktimer.entity.is_active());
    assert_eq!(ktimer.entity.expire_at(), 140);
}
