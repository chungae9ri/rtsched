extern crate std;

use super::*;
use crate::ktimer::{RtKTimer, enqueue_ktimer, init_ktimer_queue, next_ktimer};
use crate::rbtree::RBTree;
use crate::runq::{CFS_RUN_QUEUE, SchedEntity, enqueue_thread};
use crate::sched::{CURRENT_THREAD_IS_CFS, init_cfs};
use crate::test_support::TEST_LOCK;
use crate::thread::{
    CfsThread, RtThread, ThreadCtx, ThreadKind, cfs_sched_entity, rt_ktimer_entity,
};
use crate::waitq::{WAIT_QUEUE, WaitEntity};

fn cfs_thread(name: &'static str, priority: u32) -> CfsThread {
    CfsThread {
        thread: ThreadCtx {
            sp: 0,
            exc_return: 0,
            id: 1,
            name,
            state: ThreadState::Ready,
            kind: ThreadKind::Cfs,
        },
        wait_entity: WaitEntity::new(),
        sync_entity: SyncEntity::new(),
        sched_entity: SchedEntity::new(priority),
    }
}

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
        sync_entity: SyncEntity::new(),
        ktimer_entity: ptr::null_mut(),
        runtime: 0,
    }
}

unsafe fn reset_scheduler_state() {
    unsafe {
        *WAIT_QUEUE.get() = RBTree::new();
        init_ktimer_queue();
        init_cfs(100, 25);
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

unsafe fn make_running_cfs(thread: &mut CfsThread) -> ThreadHandle {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(&mut thread.thread);

        enqueue_thread(handle);
        (*CFS_RUN_QUEUE.get()).remove(cfs_sched_entity(handle));
        thread.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut thread.thread;
        CURRENT_THREAD_IS_CFS = true;

        handle
    }
}

unsafe fn make_running_rt(thread: &mut RtThread) -> ThreadHandle {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(&mut thread.thread);

        thread.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut thread.thread;
        CURRENT_THREAD_IS_CFS = false;

        handle
    }
}

unsafe fn mark_waiting_with_sync_deadline(
    thread: &mut CfsThread,
    deadline_at: u64,
) -> ThreadHandle {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(&mut thread.thread);
        thread.thread.set_state(ThreadState::Waiting);
        (*sync_entity(handle)).set_deadline_at(deadline_at);
        handle
    }
}

pub fn sync_type_wait_events_decode_named_tags() {
    assert_eq!(
        SyncType::BinarySemaphore.wait_event(),
        u32::from_be_bytes(*b"semb")
    );
    assert_eq!(
        SyncType::CountingSemaphore.wait_event(),
        u32::from_be_bytes(*b"semc")
    );
    assert_eq!(SyncType::Mutex.wait_event(), u32::from_be_bytes(*b"mute"));

    assert_eq!(
        SyncType::from_wait_event(u32::from_be_bytes(*b"semb")),
        Some(SyncType::BinarySemaphore)
    );
    assert_eq!(
        SyncType::from_wait_event(u32::from_be_bytes(*b"semc")),
        Some(SyncType::CountingSemaphore)
    );
    assert_eq!(
        SyncType::from_wait_event(u32::from_be_bytes(*b"mute")),
        Some(SyncType::Mutex)
    );
    assert_eq!(SyncType::from_wait_event(0), None);
}

pub fn sync_waiters_pop_by_earliest_deadline_not_insertion_order() {
    let mut waiters = WaitTree::new();
    let mut later = cfs_thread("later", 3);
    let mut earlier = cfs_thread("earlier", 3);

    unsafe {
        let later_handle = mark_waiting_with_sync_deadline(&mut later, 40);
        let earlier_handle = mark_waiting_with_sync_deadline(&mut earlier, 10);

        waiters.insert(sync_entity(later_handle));
        waiters.insert(sync_entity(earlier_handle));

        assert_eq!(
            pop_waiting_thread(&mut waiters).map(|thread| thread.as_ptr()),
            Some(earlier_handle.as_ptr())
        );
        assert_eq!(
            pop_waiting_thread(&mut waiters).map(|thread| thread.as_ptr()),
            Some(later_handle.as_ptr())
        );
        assert!(pop_waiting_thread(&mut waiters).is_none());
    }
}

pub fn blocking_sync_take_copies_current_scheduler_deadline() {
    let _guard = TEST_LOCK.lock().unwrap();
    let semaphore = BinarySemaphore::empty();
    let mut thread = cfs_thread("waiter", 3);

    unsafe {
        reset_scheduler_state();
        let handle = make_running_cfs(&mut thread);

        assert_eq!(semaphore.take(), Ok(()));

        assert_eq!((*sync_entity(handle)).deadline_at(), 25);

        *WAIT_QUEUE.get() = RBTree::new();
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn mutex_lock_boosts_rt_owner_deadline_and_unlock_restores_it() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mutex = Mutex::new(7);
    let mut owner = rt_thread("owner");
    let mut owner_timer = RtKTimer::new(100, ptr::null_mut(), "owner");
    let mut waiter = rt_thread("waiter");
    let mut waiter_timer = RtKTimer::new(10, ptr::null_mut(), "waiter");

    unsafe {
        reset_scheduler_state();
        owner_timer.init_rt_ktimer(&mut owner.thread);
        waiter_timer.init_rt_ktimer(&mut waiter.thread);
        enqueue_ktimer(owner_timer.entity_mut());
        enqueue_ktimer(waiter_timer.entity_mut());
        make_running_rt(&mut owner);
    }

    let owner_guard = mutex.lock().expect("owner should lock mutex");

    let waiter_handle = unsafe {
        owner.thread.set_state(ThreadState::Ready);
        make_running_rt(&mut waiter)
    };

    let waiter_guard = mutex.lock().expect("waiter should block on mutex");

    unsafe {
        assert_eq!(waiter.thread.state, ThreadState::Waiting);
        assert_eq!((*sync_entity(waiter_handle)).deadline_at(), 10);
        assert_eq!(owner_timer.entity.expire_at(), 10);
        assert!(ptr::eq(next_ktimer(), owner_timer.entity_mut()));

        core::mem::forget(waiter_guard);
        owner.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut owner.thread;
        CURRENT_THREAD_IS_CFS = false;
    }

    drop(owner_guard);

    unsafe {
        assert_eq!(owner_timer.entity.expire_at(), 100);
        assert_eq!(waiter.thread.state, ThreadState::Ready);
        assert!(ptr::eq(
            rt_ktimer_entity(waiter_handle),
            waiter_timer.entity_mut()
        ));

        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn mutex_lock_boosts_rt_owner_to_earliest_scheduler_deadline_and_unlock_restores_it() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mutex = Mutex::new(7);
    let mut owner = rt_thread("owner");
    let mut owner_timer = RtKTimer::new(100, ptr::null_mut(), "owner");
    let mut waiter = rt_thread("waiter");
    let mut waiter_timer = RtKTimer::new(40, ptr::null_mut(), "waiter");

    unsafe {
        reset_scheduler_state();
        owner_timer.init_rt_ktimer(&mut owner.thread);
        waiter_timer.init_rt_ktimer(&mut waiter.thread);
        enqueue_ktimer(owner_timer.entity_mut());
        enqueue_ktimer(waiter_timer.entity_mut());
        make_running_rt(&mut owner);
    }

    let owner_guard = mutex.lock().expect("owner should lock mutex");

    let waiter_handle = unsafe {
        owner.thread.set_state(ThreadState::Ready);
        make_running_rt(&mut waiter)
    };

    let waiter_guard = mutex.lock().expect("waiter should block on mutex");

    unsafe {
        assert_eq!(waiter.thread.state, ThreadState::Waiting);
        assert_eq!((*sync_entity(waiter_handle)).deadline_at(), 40);
        assert_eq!(owner_timer.entity.expire_at(), 25);

        core::mem::forget(waiter_guard);
        owner.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut owner.thread;
        CURRENT_THREAD_IS_CFS = false;
    }

    drop(owner_guard);

    unsafe {
        assert_eq!(owner_timer.entity.expire_at(), 100);
        assert_eq!(waiter.thread.state, ThreadState::Ready);

        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn mutex_lock_boosts_cfs_owner_to_earliest_scheduler_deadline_and_unlock_restores_it() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mutex = Mutex::new(7);
    let mut owner = cfs_thread("owner", 3);
    let mut waiter = rt_thread("waiter");
    let mut waiter_timer = RtKTimer::new(10, ptr::null_mut(), "waiter");
    let mut earlier_rt = rt_thread("earlier_rt");
    let mut earlier_rt_timer = RtKTimer::new(8, ptr::null_mut(), "earlier_rt");

    unsafe {
        reset_scheduler_state();
        waiter_timer.init_rt_ktimer(&mut waiter.thread);
        earlier_rt_timer.init_rt_ktimer(&mut earlier_rt.thread);
        enqueue_ktimer(waiter_timer.entity_mut());
        enqueue_ktimer(earlier_rt_timer.entity_mut());
        make_running_cfs(&mut owner);
    }

    let owner_guard = mutex.lock().expect("owner should lock mutex");

    unsafe {
        owner.thread.set_state(ThreadState::Ready);
        make_running_rt(&mut waiter);
    }

    let waiter_guard = mutex.lock().expect("waiter should block on mutex");

    unsafe {
        assert_eq!(waiter.thread.state, ThreadState::Waiting);
        let cfs = ptr::addr_of!(CFS_KTIMER);
        let cfs_entity = ptr::addr_of!((*cfs).entity);
        assert_eq!((*cfs_entity).expire_at(), 8);

        core::mem::forget(waiter_guard);
        owner.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut owner.thread;
        CURRENT_THREAD_IS_CFS = true;
    }

    drop(owner_guard);

    unsafe {
        let cfs = ptr::addr_of!(CFS_KTIMER);
        let cfs_entity = ptr::addr_of!((*cfs).entity);
        assert_eq!((*cfs_entity).expire_at(), 25);
        assert_eq!(waiter.thread.state, ThreadState::Ready);

        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn binary_semaphore_try_take_and_give_track_single_token() {
    let semaphore = BinarySemaphore::available();

    assert_eq!(semaphore.try_take(), Ok(()));
    assert!(!semaphore.is_available());
    assert_eq!(semaphore.try_take(), Err(SemaphoreError::WouldBlock));

    assert_eq!(semaphore.give(), Ok(()));
    assert!(semaphore.is_available());
    assert_eq!(semaphore.give(), Err(SemaphoreError::Full));
}

pub fn binary_semaphore_take_blocks_current_cfs_thread_until_give() {
    let _guard = TEST_LOCK.lock().unwrap();
    let semaphore = BinarySemaphore::empty();
    let mut thread = cfs_thread("waiter", 3);

    unsafe {
        reset_scheduler_state();
        let handle = make_running_cfs(&mut thread);

        assert_eq!(semaphore.take(), Ok(()));

        assert_eq!(thread.thread.state, ThreadState::Waiting);
        assert!(!semaphore.is_available());
        assert!((*WAIT_QUEUE.get()).contains(wait_entity(handle)));
        assert_eq!(
            (*wait_entity(handle)).waitevt,
            Some(SyncType::BinarySemaphore)
        );

        assert_eq!(semaphore.give(), Ok(()));

        assert_eq!(thread.thread.state, ThreadState::Ready);
        assert!(!semaphore.is_available());
        assert!(!(*WAIT_QUEUE.get()).contains(wait_entity(handle)));
        assert!((*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(handle)));

        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn binary_semaphore_take_without_current_thread_reports_error() {
    let _guard = TEST_LOCK.lock().unwrap();
    let semaphore = BinarySemaphore::empty();

    unsafe {
        reset_scheduler_state();
    }

    assert_eq!(semaphore.take(), Err(SemaphoreError::NoCurrentThread));
}

pub fn counting_semaphore_try_take_and_give_track_bounded_tokens() {
    let semaphore = CountingSemaphore::new(3, 2);

    assert_eq!(semaphore.max_count(), 3);
    assert_eq!(semaphore.count(), 2);

    assert_eq!(semaphore.try_take(), Ok(()));
    assert_eq!(semaphore.count(), 1);
    assert_eq!(semaphore.try_take(), Ok(()));
    assert_eq!(semaphore.count(), 0);
    assert_eq!(semaphore.try_take(), Err(SemaphoreError::WouldBlock));

    assert_eq!(semaphore.give(), Ok(()));
    assert_eq!(semaphore.count(), 1);
    assert_eq!(semaphore.give(), Ok(()));
    assert_eq!(semaphore.count(), 2);
    assert_eq!(semaphore.give(), Ok(()));
    assert_eq!(semaphore.count(), 3);
    assert_eq!(semaphore.give(), Err(SemaphoreError::Full));
}

pub fn counting_semaphore_take_blocks_current_cfs_thread_until_give() {
    let _guard = TEST_LOCK.lock().unwrap();
    let semaphore = CountingSemaphore::empty(3);
    let mut thread = cfs_thread("waiter", 3);

    unsafe {
        reset_scheduler_state();
        let handle = make_running_cfs(&mut thread);

        assert_eq!(semaphore.take(), Ok(()));

        assert_eq!(thread.thread.state, ThreadState::Waiting);
        assert_eq!(semaphore.count(), 0);
        assert!((*WAIT_QUEUE.get()).contains(wait_entity(handle)));
        assert_eq!(
            (*wait_entity(handle)).waitevt,
            Some(SyncType::CountingSemaphore)
        );

        assert_eq!(semaphore.give(), Ok(()));

        assert_eq!(thread.thread.state, ThreadState::Ready);
        assert_eq!(semaphore.count(), 0);
        assert!(!(*WAIT_QUEUE.get()).contains(wait_entity(handle)));
        assert!((*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(handle)));

        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn counting_semaphore_take_without_current_thread_reports_error() {
    let _guard = TEST_LOCK.lock().unwrap();
    let semaphore = CountingSemaphore::empty(2);

    unsafe {
        reset_scheduler_state();
    }

    assert_eq!(semaphore.take(), Err(SemaphoreError::NoCurrentThread));
}

pub fn counting_semaphore_rejects_zero_max_count() {
    let _ = CountingSemaphore::empty(0);
}

pub fn counting_semaphore_rejects_initial_count_above_max_count() {
    let _ = CountingSemaphore::new(2, 3);
}

pub fn mutex_try_lock_protects_data_and_unlocks_on_drop() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mutex = Mutex::new(41);
    let mut thread = cfs_thread("owner", 3);

    unsafe {
        reset_scheduler_state();
        make_running_cfs(&mut thread);
    }

    {
        let mut guard = mutex.try_lock().expect("mutex should lock");
        *guard += 1;

        assert_eq!(*guard, 42);
        assert!(mutex.is_locked());
        assert!(matches!(mutex.try_lock(), Err(MutexError::WouldDeadlock)));
    }

    assert!(!mutex.is_locked());
    assert_eq!(*mutex.lock().expect("mutex should lock again"), 42);

    unsafe {
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn mutex_try_lock_reports_would_block_for_other_owner() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mutex = Mutex::new(7);
    let mut owner = cfs_thread("owner", 3);
    let mut other = cfs_thread("other", 5);

    unsafe {
        reset_scheduler_state();
        make_running_cfs(&mut owner);
    }

    let guard = mutex.try_lock().expect("owner should lock mutex");

    unsafe {
        owner.thread.set_state(ThreadState::Ready);
        other.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut other.thread;
        CURRENT_THREAD_IS_CFS = true;
    }

    assert!(matches!(mutex.try_lock(), Err(MutexError::WouldBlock)));

    unsafe {
        other.thread.set_state(ThreadState::Ready);
        owner.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut owner.thread;
        CURRENT_THREAD_IS_CFS = true;
    }

    drop(guard);
    assert!(!mutex.is_locked());

    unsafe {
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn mutex_lock_blocks_waiter_and_drop_transfers_ownership() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mutex = Mutex::new(7);
    let mut owner = cfs_thread("owner", 3);
    let mut waiter = cfs_thread("waiter", 5);

    unsafe {
        reset_scheduler_state();
        make_running_cfs(&mut owner);
    }

    let owner_guard = mutex.lock().expect("owner should lock mutex");

    unsafe {
        owner.thread.set_state(ThreadState::Ready);
        let waiter_handle = make_running_cfs(&mut waiter);
        let waiter_guard = mutex.lock().expect("waiter should block on mutex");

        assert_eq!(waiter.thread.state, ThreadState::Waiting);
        assert!((*WAIT_QUEUE.get()).contains(wait_entity(waiter_handle)));
        assert_eq!((*wait_entity(waiter_handle)).waitevt, Some(SyncType::Mutex));

        core::mem::forget(waiter_guard);

        owner.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut owner.thread;
        CURRENT_THREAD_IS_CFS = true;
    }

    drop(owner_guard);

    unsafe {
        let waiter_handle = ThreadHandle::from_thread_ctx(&mut waiter.thread);

        assert_eq!(waiter.thread.state, ThreadState::Ready);
        assert!(!(*WAIT_QUEUE.get()).contains(wait_entity(waiter_handle)));
        assert!((*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(waiter_handle)));

        owner.thread.set_state(ThreadState::Ready);
        waiter.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut waiter.thread;
        CURRENT_THREAD_IS_CFS = true;
    }

    assert!(mutex.is_locked());
    assert!(matches!(mutex.try_lock(), Err(MutexError::WouldDeadlock)));

    unsafe {
        CURRENT_THREAD_CTX = ptr::null_mut();
        CURRENT_THREAD_IS_CFS = false;
    }
}

pub fn mutex_lock_without_current_thread_reports_error() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mutex = Mutex::new(7);

    unsafe {
        reset_scheduler_state();
    }

    assert!(matches!(mutex.try_lock(), Err(MutexError::NoCurrentThread)));
    assert!(matches!(mutex.lock(), Err(MutexError::NoCurrentThread)));
}

pub fn mutex_unique_access_helpers_skip_scheduler_locking() {
    let mut mutex = Mutex::new(1);

    *mutex.get_mut() = 2;

    assert_eq!(mutex.into_inner(), 2);
}
