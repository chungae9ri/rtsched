extern crate std;

use super::*;
use crate::diagnostics::sched::traverse_idle_thread_fn;
use crate::ktimer::{
    RtKTimer, clear_elapsed_ticks_since_last_interrupt_for_test, init_ktimer_queue,
    set_elapsed_ticks_since_last_interrupt_for_test,
};
use crate::test_support::TEST_LOCK;
use crate::thread::{CfsThread, IdleThread, RtThread, ThreadHandle, ThreadKind};
use crate::waitq::WaitEntity;

fn cfs_thread(name: &'static str, priority: u32, vruntime: u64, state: ThreadState) -> CfsThread {
    let mut thread = CfsThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 1,
            name,
            state,
            kind: ThreadKind::Cfs,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: crate::sync::SyncEntity::new(),
        sched_entity: SchedEntity::new(priority),
    };
    thread.sched_entity.vruntime = vruntime;
    thread
}

fn rt_thread(name: &'static str) -> RtThread {
    RtThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 2,
            name,
            state: ThreadState::Running,
            kind: ThreadKind::Rt,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: crate::sync::SyncEntity::new(),
        ktimer_entity: ptr::null_mut(),
        runtime: 0,
    }
}

fn idle_thread(name: &'static str, state: ThreadState) -> IdleThread {
    IdleThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 0,
            name,
            state,
            kind: ThreadKind::Idle,
        },
    }
}

unsafe fn reset_sched_state() -> *mut KTimerEntity {
    unsafe {
        init_ktimer_queue();
        init_cfs_rq();
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
        IDLE_THREAD_CTX = ptr::null_mut();
        CFS_KTIMER = CfsKTimer::new(100, 25, "cfs");
        (*ptr::addr_of_mut!(CFS_KTIMER)).entity_mut()
    }
}

struct ElapsedTicksOverrideGuard;

impl Drop for ElapsedTicksOverrideGuard {
    fn drop(&mut self) {
        clear_elapsed_ticks_since_last_interrupt_for_test();
    }
}

unsafe fn schedule_for_test(next_ktimer: *mut KTimerEntity, elapsed: u32) {
    unsafe {
        update_next_ktimer(next_ktimer);
        set_elapsed_ticks_since_last_interrupt_for_test(elapsed);
        let _guard = ElapsedTicksOverrideGuard;

        schedule();
    }
}

unsafe fn queue_cfs_thread(thread: *mut ThreadCtx) {
    unsafe {
        let entity = cfs_sched_entity(ThreadHandle::from_thread_ctx(thread));
        (*entity).reset_links();
        (*CFS_RUN_QUEUE.get()).insert(entity);
        *CFS_RUN_QUEUE.priority_sum() += (*entity).priority;
    }
}

unsafe fn thread_handle(thread: *mut ThreadCtx) -> ThreadHandle {
    unsafe { ThreadHandle::from_thread_ctx(thread) }
}

unsafe fn running_thread_count(threads: &[*const ThreadCtx]) -> usize {
    threads
        .iter()
        .filter(|&&thread| !thread.is_null() && unsafe { (*thread).state == ThreadState::Running })
        .count()
}

pub fn scheduler_started_latch_is_marked_explicitly() {
    let _guard = TEST_LOCK.lock().unwrap();

    reset_scheduler_started();
    assert!(!scheduler_started());

    mark_scheduler_started();
    assert!(scheduler_started());

    reset_scheduler_started();
}

pub fn handle_sched_tick_ignores_queued_work_before_scheduler_start() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut cfs = cfs_thread("cfs", 1, 0, ThreadState::Ready);
    let mut rt = rt_thread("rt");
    let mut rt_ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");
    rt.thread.state = ThreadState::Ready;

    unsafe {
        let _ = reset_sched_state();
        queue_cfs_thread(&mut cfs.thread);
        rt_ktimer.init_rt_ktimer(&mut rt.thread);
        enqueue_ktimer(rt_ktimer.entity_mut());
        reset_scheduler_started();
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }

    handle_sched_tick();

    assert_eq!(cfs.thread.state, ThreadState::Ready);
    assert_eq!(rt.thread.state, ThreadState::Ready);
    unsafe {
        assert!(CURRENT_THREAD_CTX.is_null());
        assert!(!CURRENT_THREAD_IS_CFS);
        assert!((*CFS_RUN_QUEUE.get()).contains((&cfs.sched_entity) as *const SchedEntity));
        assert!(ptr::eq(next_ktimer(), rt_ktimer.entity_mut()));
        assert!(!scheduler_started());
    }
}

pub fn init_cfs_resets_run_queue_and_configures_cfs_timer() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        init_ktimer_queue();
        init_cfs(100, 25);
    }

    unsafe {
        assert_eq!((*CFS_RUN_QUEUE.get()).len(), 0);
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), 0);
        let cfs = ptr::addr_of!(CFS_KTIMER);
        let entity = ptr::addr_of!((*cfs).entity);
        assert_eq!((*cfs).period_ticks(), 100);
        assert_eq!((*entity).expire(), 25);
        assert_eq!((*cfs).execution_ticks(), 25);
        assert!((*entity).is_active());
    }
}

pub fn cfs_accounting_updates_vruntime_and_sched_ticks() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut current = cfs_thread("current", 1, 0, ThreadState::Running);
    let mut queued = cfs_thread("queued", 3, 100, ThreadState::Ready);

    unsafe {
        let cfs_ktimer = reset_sched_state();
        CURRENT_THREAD_CTX = &mut current.thread;
        CURRENT_THREAD_IS_CFS = true;
        queue_cfs_thread(&mut queued.thread);
        *CFS_RUN_QUEUE.priority_sum() += current.sched_entity.priority;

        schedule_for_test(cfs_ktimer, 12);
    }

    assert_eq!(current.sched_entity.sched_tick_cnt(), 12);
    assert_eq!(current.sched_entity.vruntime(), 3);
    assert!(current.thread.state == ThreadState::Running);
}

pub fn cfs_preempts_current_when_queued_thread_has_lower_vruntime() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut current = cfs_thread("current", 2, 10, ThreadState::Running);
    let mut queued = cfs_thread("queued", 2, 5, ThreadState::Ready);

    unsafe {
        let cfs_ktimer = reset_sched_state();
        CURRENT_THREAD_CTX = &mut current.thread;
        CURRENT_THREAD_IS_CFS = true;
        queue_cfs_thread(&mut queued.thread);
        *CFS_RUN_QUEUE.priority_sum() += current.sched_entity.priority;

        schedule_for_test(cfs_ktimer, 10);
    }

    assert_eq!(current.sched_entity.vruntime(), 15);
    assert!(current.thread.state == ThreadState::Ready);
    assert!(queued.thread.state == ThreadState::Running);
    assert_eq!(
        unsafe { running_thread_count(&[&current.thread, &queued.thread]) },
        1
    );
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &queued.thread));
        assert!(CURRENT_THREAD_IS_CFS);
        assert!(ptr::eq(
            (*CFS_RUN_QUEUE.get()).first(),
            &current.sched_entity
        ));
    }
}

pub fn cfs_keeps_current_when_it_still_has_lower_vruntime() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut current = cfs_thread("current", 2, 0, ThreadState::Running);
    let mut queued = cfs_thread("queued", 2, 10, ThreadState::Ready);

    unsafe {
        let cfs_ktimer = reset_sched_state();
        CURRENT_THREAD_CTX = &mut current.thread;
        CURRENT_THREAD_IS_CFS = true;
        queue_cfs_thread(&mut queued.thread);
        *CFS_RUN_QUEUE.priority_sum() += current.sched_entity.priority;

        schedule_for_test(cfs_ktimer, 2);
    }

    assert_eq!(current.sched_entity.vruntime(), 1);
    assert!(current.thread.state == ThreadState::Running);
    assert!(queued.thread.state == ThreadState::Ready);
    assert_eq!(
        unsafe { running_thread_count(&[&current.thread, &queued.thread]) },
        1
    );
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &current.thread));
        assert!(CURRENT_THREAD_IS_CFS);
        assert!(ptr::eq(
            (*CFS_RUN_QUEUE.get()).first(),
            &queued.sched_entity
        ));
        assert!(!current.sched_entity.is_linked());
    }
}

pub fn cfs_timer_switches_from_rt_thread_to_leftmost_cfs_thread() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut rt = rt_thread("rt");
    let mut first = cfs_thread("first", 1, 30, ThreadState::Ready);
    let mut second = cfs_thread("second", 1, 10, ThreadState::Ready);

    unsafe {
        let cfs_ktimer = reset_sched_state();
        CURRENT_THREAD_CTX = &mut rt.thread;
        CURRENT_THREAD_IS_CFS = false;
        queue_cfs_thread(&mut first.thread);
        queue_cfs_thread(&mut second.thread);

        schedule_for_test(cfs_ktimer, 0);
    }

    assert!(rt.thread.state == ThreadState::Ready);
    assert!(second.thread.state == ThreadState::Running);
    assert_eq!(
        unsafe { running_thread_count(&[&rt.thread, &first.thread, &second.thread]) },
        1
    );
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &second.thread));
        assert!(CURRENT_THREAD_IS_CFS);
        assert!(ptr::eq((*CFS_RUN_QUEUE.get()).first(), &first.sched_entity));
    }
}

pub fn rt_timer_switches_from_cfs_thread_to_rt_thread_and_requeues_cfs() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut current = cfs_thread("cfs", 2, 4, ThreadState::Running);
    let mut rt = rt_thread("rt");
    let mut rt_ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

    unsafe {
        let _ = reset_sched_state();
        rt_ktimer.init_rt_ktimer(&mut rt.thread);
        CURRENT_THREAD_CTX = &mut current.thread;
        CURRENT_THREAD_IS_CFS = true;
        *CFS_RUN_QUEUE.priority_sum() = current.sched_entity.priority;

        schedule_for_test(rt_ktimer.entity_mut(), 0);
    }

    assert!(current.thread.state == ThreadState::Ready);
    assert!(rt.thread.state == ThreadState::Running);
    assert_eq!(
        unsafe { running_thread_count(&[&current.thread, &rt.thread]) },
        1
    );
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &rt.thread));
        assert!(!CURRENT_THREAD_IS_CFS);
        assert!(ptr::eq(
            (*CFS_RUN_QUEUE.get()).first(),
            &current.sched_entity
        ));
    }
}

pub fn register_idle_thread_rejects_cfs_and_rt_threads() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let _guard = TEST_LOCK.lock().unwrap();

    let mut cfs = cfs_thread("cfs", 16, 0, ThreadState::Ready);
    let mut rt = rt_thread("rt");

    unsafe {
        let _ = reset_sched_state();

        let cfs_result = catch_unwind(AssertUnwindSafe(|| {
            register_idle_thread(thread_handle(&mut cfs.thread));
        }));
        let rt_result = catch_unwind(AssertUnwindSafe(|| {
            register_idle_thread(thread_handle(&mut rt.thread));
        }));

        assert!(cfs_result.is_err());
        assert!(rt_result.is_err());
        assert!(IDLE_THREAD_CTX.is_null());
    }
}

pub fn cfs_timer_switches_from_rt_thread_to_idle_when_run_queue_is_empty() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut rt = rt_thread("rt");

    unsafe {
        let cfs_ktimer = reset_sched_state();
        register_idle_thread(thread_handle(&mut idle.thread));
        CURRENT_THREAD_CTX = &mut rt.thread;
        CURRENT_THREAD_IS_CFS = false;

        schedule_for_test(cfs_ktimer, 0);
    }

    assert!(idle.thread.state == ThreadState::Running);
    assert!(rt.thread.state == ThreadState::Ready);
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &idle.thread));
        assert!(!CURRENT_THREAD_IS_CFS);
        assert_eq!((*CFS_RUN_QUEUE.get()).len(), 0);
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), 0);
    }
}

pub fn null_next_timer_switches_from_rt_thread_to_idle() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut rt = rt_thread("rt");

    unsafe {
        let _ = reset_sched_state();
        register_idle_thread(thread_handle(&mut idle.thread));
        CURRENT_THREAD_CTX = &mut rt.thread;
        CURRENT_THREAD_IS_CFS = false;

        schedule_for_test(ptr::null_mut(), 0);
    }

    assert!(idle.thread.state == ThreadState::Running);
    assert!(rt.thread.state == ThreadState::Ready);
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &idle.thread));
        assert!(!CURRENT_THREAD_IS_CFS);
        assert_eq!((*CFS_RUN_QUEUE.get()).len(), 0);
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), 0);
    }
}

pub fn cfs_timer_switches_from_current_cfs_to_idle_when_run_queue_is_empty() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut current = cfs_thread("current", 2, 0, ThreadState::Running);

    unsafe {
        let cfs_ktimer = reset_sched_state();
        register_idle_thread(thread_handle(&mut idle.thread));
        CURRENT_THREAD_CTX = &mut current.thread;
        CURRENT_THREAD_IS_CFS = true;
        *CFS_RUN_QUEUE.priority_sum() = current.sched_entity.priority;

        schedule_for_test(cfs_ktimer, 10);
    }

    assert!(idle.thread.state == ThreadState::Running);
    assert!(current.thread.state == ThreadState::Ready);
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &idle.thread));
        assert!(!CURRENT_THREAD_IS_CFS);
        assert!(ptr::eq(
            (*CFS_RUN_QUEUE.get()).first(),
            &current.sched_entity
        ));
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), current.sched_entity.priority);
    }
}

pub fn inactive_cfs_timer_falls_back_to_idle_without_popping_queued_cfs_thread() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut rt = rt_thread("rt");
    let mut queued = cfs_thread("queued", 1, 0, ThreadState::Ready);

    unsafe {
        let cfs_ktimer = reset_sched_state();
        (*cfs_ktimer).set_active(false);
        register_idle_thread(thread_handle(&mut idle.thread));
        CURRENT_THREAD_CTX = &mut rt.thread;
        CURRENT_THREAD_IS_CFS = false;
        queue_cfs_thread(&mut queued.thread);

        schedule_for_test(cfs_ktimer, 0);
    }

    assert!(idle.thread.state == ThreadState::Running);
    assert!(rt.thread.state == ThreadState::Ready);
    assert!(queued.thread.state == ThreadState::Ready);
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &idle.thread));
        assert!(!CURRENT_THREAD_IS_CFS);
        assert!(ptr::eq(
            (*CFS_RUN_QUEUE.get()).first(),
            &queued.sched_entity
        ));
    }
}

pub fn idle_thread_yields_to_queued_cfs_thread_without_entering_run_queue() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut queued = cfs_thread("queued", 1, 0, ThreadState::Ready);

    unsafe {
        let cfs_ktimer = reset_sched_state();
        register_idle_thread(thread_handle(&mut idle.thread));
        idle.thread.state = ThreadState::Running;
        CURRENT_THREAD_CTX = &mut idle.thread;
        CURRENT_THREAD_IS_CFS = false;
        queue_cfs_thread(&mut queued.thread);

        schedule_for_test(cfs_ktimer, 0);
    }

    assert!(idle.thread.state == ThreadState::Ready);
    assert!(queued.thread.state == ThreadState::Running);
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &queued.thread));
        assert!(CURRENT_THREAD_IS_CFS);
        assert_eq!((*CFS_RUN_QUEUE.get()).len(), 0);
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), queued.sched_entity.priority);
    }
}

pub fn rt_timer_switches_from_idle_thread_without_requeueing_idle() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut rt = rt_thread("rt");
    let mut rt_ktimer = RtKTimer::new(50, ptr::null_mut(), "rt");

    unsafe {
        let _ = reset_sched_state();
        register_idle_thread(thread_handle(&mut idle.thread));
        idle.thread.state = ThreadState::Running;
        rt_ktimer.init_rt_ktimer(&mut rt.thread);
        CURRENT_THREAD_CTX = &mut idle.thread;
        CURRENT_THREAD_IS_CFS = false;

        schedule_for_test(rt_ktimer.entity_mut(), 0);
    }

    assert!(idle.thread.state == ThreadState::Ready);
    assert!(rt.thread.state == ThreadState::Running);
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &rt.thread));
        assert!(!CURRENT_THREAD_IS_CFS);
        assert_eq!((*CFS_RUN_QUEUE.get()).len(), 0);
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), 0);
    }
}

pub fn idle_fallback_requeues_current_cfs_thread() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut current = cfs_thread("current", 2, 0, ThreadState::Running);

    unsafe {
        let _ = reset_sched_state();
        register_idle_thread(thread_handle(&mut idle.thread));
        CURRENT_THREAD_CTX = &mut current.thread;
        CURRENT_THREAD_IS_CFS = true;
        *CFS_RUN_QUEUE.priority_sum() = current.sched_entity.priority;

        switch_to_idle_thread_with_current_requeued();
    }

    assert!(idle.thread.state == ThreadState::Running);
    assert!(current.thread.state == ThreadState::Ready);
    unsafe {
        assert!(ptr::eq(CURRENT_THREAD_CTX, &idle.thread));
        assert!(!CURRENT_THREAD_IS_CFS);
        assert!(ptr::eq(
            (*CFS_RUN_QUEUE.get()).first(),
            &current.sched_entity
        ));
        assert_eq!(*CFS_RUN_QUEUE.priority_sum(), current.sched_entity.priority);
    }
}

pub fn traverse_idle_thread_visits_registered_idle_thread() {
    let _guard = TEST_LOCK.lock().unwrap();

    let mut idle = idle_thread("idle", ThreadState::Ready);
    let mut seen = ptr::null();

    unsafe {
        let _ = reset_sched_state();
        register_idle_thread(thread_handle(&mut idle.thread));
    }

    traverse_idle_thread_fn(|thread| {
        seen = thread as *const ThreadCtx;
    });

    assert!(ptr::eq(seen, &idle.thread));
}
