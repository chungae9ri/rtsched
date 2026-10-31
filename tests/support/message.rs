extern crate std;

use super::*;
use crate::ktimer::{RtKTimer, enqueue_ktimer, init_ktimer_queue, with_ktimer_queue};
use crate::runq::{CFS_RUN_QUEUE, enqueue_thread};
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS, init_cfs};
use crate::test_support::{TEST_LOCK, cfs_thread, rt_thread};
use crate::thread::{CfsThread, RtThread, ThreadHandle, cfs_sched_entity, rt_ktimer_entity};
use crate::waitq::{WAIT_QUEUE, wait_entity};
use core::ptr;

struct SchedulerReset;

impl SchedulerReset {
    fn new() -> Self {
        Self::reset();
        Self
    }

    fn reset() {
        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
            WAIT_QUEUE.reset_for_test();
            init_ktimer_queue();
            init_cfs(1_000, 100);
        }
    }
}

impl Drop for SchedulerReset {
    fn drop(&mut self) {
        Self::reset();
    }
}

unsafe fn start_cfs(thread: &mut CfsThread) -> ThreadHandle {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(&mut thread.thread);
        enqueue_thread(handle);
        resume_cfs(thread);
        handle
    }
}

unsafe fn resume_cfs(thread: &mut CfsThread) {
    unsafe {
        let handle = ThreadHandle::from_thread_ctx(&mut thread.thread);
        (*CFS_RUN_QUEUE.get()).remove(cfs_sched_entity(handle));
        thread.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut thread.thread;
        CURRENT_THREAD_IS_CFS = true;
    }
}

unsafe fn resume_rt(thread: &mut RtThread) -> ThreadHandle {
    unsafe {
        thread.thread.set_state(ThreadState::Running);
        CURRENT_THREAD_CTX = &mut thread.thread;
        CURRENT_THREAD_IS_CFS = false;
        ThreadHandle::from_thread_ctx(&mut thread.thread)
    }
}

pub fn cfs_receiver_suspends_and_resumes_with_message() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut receiver = cfs_thread("receiver", 1);
    let _reset = SchedulerReset::new();
    let handle = unsafe { start_cfs(&mut receiver) };
    let mut suspensions = 0;
    let value = queue.receive_with_wait(true, || {
        suspensions += 1;
        assert_eq!(receiver.thread.state, ThreadState::Suspended);
        unsafe {
            assert_eq!(
                (*wait_entity(handle)).waitevt,
                Some(SyncType::MessageQueueReceive)
            );
            assert!((*WAIT_QUEUE.get()).is_empty());
            assert!(!(*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(handle)));
        }
        assert_eq!(queue.try_send(42), Ok(()));
        assert_eq!(receiver.thread.state, ThreadState::Ready);
        unsafe { resume_cfs(&mut receiver) };
    });
    assert_eq!(value, Ok(42));
    assert_eq!(suspensions, 1);
    assert!(queue.is_empty());
    unsafe { assert!((*queue.state.get()).receivers.is_empty()) };
}

pub fn cfs_sender_suspends_until_space_is_available() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut sender = cfs_thread("sender", 1);
    let _reset = SchedulerReset::new();
    unsafe { start_cfs(&mut sender) };
    queue.try_send(1).unwrap();
    let mut suspensions = 0;
    let result = queue.send_with_wait(2, true, || {
        suspensions += 1;
        assert_eq!(sender.thread.state, ThreadState::Suspended);
        assert_eq!(queue.try_receive(), Ok(1));
        assert_eq!(sender.thread.state, ThreadState::Ready);
        unsafe { resume_cfs(&mut sender) };
    });
    assert_eq!(result, Ok(()));
    assert_eq!(suspensions, 1);
    assert_eq!(queue.try_receive(), Ok(2));
}

pub fn rt_receiver_retries_after_another_consumer_takes_the_message() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut receiver = rt_thread("receiver");
    let mut timer = RtKTimer::new(50, ptr::null_mut(), "receiver");
    let _reset = SchedulerReset::new();
    unsafe {
        timer.init_rt_ktimer(&mut receiver.thread);
        enqueue_ktimer(timer.entity_mut());
        resume_rt(&mut receiver);
    }
    let mut suspensions = 0;
    let value = queue.receive_with_wait(true, || {
        suspensions += 1;
        assert!(suspensions <= 2);
        assert_eq!(receiver.thread.state, ThreadState::Suspended);
        with_ktimer_queue(|timers| assert!(!timers.contains(timer.entity_mut())));
        queue.try_send(suspensions).unwrap();
        assert_eq!(receiver.thread.state, ThreadState::Ready);
        with_ktimer_queue(|timers| assert!(timers.contains(timer.entity_mut())));
        if suspensions == 1 {
            assert_eq!(queue.try_receive(), Ok(1));
        }
        unsafe { resume_rt(&mut receiver) };
    });
    assert_eq!(value, Ok(2));
    assert_eq!(suspensions, 2);
    assert_eq!(receiver.thread.state, ThreadState::Running);
    assert_eq!(timer.miss_cnt, 0);
}

pub fn rt_sender_retries_after_another_producer_fills_the_slot() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut sender = rt_thread("sender");
    let mut timer = RtKTimer::new(50, ptr::null_mut(), "sender");
    let _reset = SchedulerReset::new();
    unsafe {
        timer.init_rt_ktimer(&mut sender.thread);
        enqueue_ktimer(timer.entity_mut());
        resume_rt(&mut sender);
    }
    queue.try_send(1).unwrap();
    let mut suspensions = 0;
    let result = queue.send_with_wait(3, true, || {
        suspensions += 1;
        assert!(suspensions <= 2);
        assert_eq!(sender.thread.state, ThreadState::Suspended);
        assert_eq!(queue.try_receive(), Ok(suspensions));
        assert_eq!(sender.thread.state, ThreadState::Ready);
        if suspensions == 1 {
            queue.try_send(2).unwrap();
        }
        unsafe { resume_rt(&mut sender) };
    });
    assert_eq!(result, Ok(()));
    assert_eq!(suspensions, 2);
    assert_eq!(queue.try_receive(), Ok(3));
    assert_eq!(sender.thread.state, ThreadState::Running);
}

pub fn queue_waiters_wake_by_deadline_and_address_in_both_directions() {
    let _lock = TEST_LOCK.lock().unwrap();
    for sending in [false, true] {
        let queue = MessageQueue::<u32, 3>::new();
        let mut threads = [
            rt_thread("late"),
            rt_thread("early_a"),
            rt_thread("early_b"),
        ];
        let mut timers = [
            RtKTimer::new(80, ptr::null_mut(), "late"),
            RtKTimer::new(20, ptr::null_mut(), "early_a"),
            RtKTimer::new(20, ptr::null_mut(), "early_b"),
        ];
        let _reset = SchedulerReset::new();
        if sending {
            for value in 0..3 {
                queue.try_send(value).unwrap();
            }
        }
        for (thread, timer) in threads.iter_mut().zip(&mut timers) {
            unsafe {
                timer.init_rt_ktimer(&mut thread.thread);
                enqueue_ktimer(timer.entity_mut());
                resume_rt(thread);
            }
            critical_section(|| {
                queue.block(
                    if sending {
                        SyncType::MessageQueueSend
                    } else {
                        SyncType::MessageQueueReceive
                    },
                    true,
                )
            })
            .unwrap();
        }
        for (step, index) in [1, 2, 0].into_iter().enumerate() {
            if sending {
                assert_eq!(queue.try_receive(), Ok(step as u32));
            } else {
                queue.try_send(step as u32).unwrap();
            }
            assert_eq!(threads[index].thread.state, ThreadState::Ready);
            assert_eq!(
                threads
                    .iter()
                    .filter(|thread| thread.thread.state == ThreadState::Ready)
                    .count(),
                step + 1
            );
        }
        for thread in &mut threads {
            let handle = unsafe { ThreadHandle::from_thread_ctx(&mut thread.thread) };
            with_ktimer_queue(|timers| {
                assert!(timers.contains(unsafe { rt_ktimer_entity(handle) }))
            });
        }
    }
}

pub fn queues_do_not_wake_each_others_waiters() {
    let _lock = TEST_LOCK.lock().unwrap();
    let first = MessageQueue::<u32, 1>::new();
    let second = MessageQueue::<u32, 1>::new();
    let mut receiver = cfs_thread("receiver", 1);
    let _reset = SchedulerReset::new();
    unsafe { start_cfs(&mut receiver) };
    critical_section(|| first.block(SyncType::MessageQueueReceive, true)).unwrap();
    second.try_send(1).unwrap();
    assert_eq!(receiver.thread.state, ThreadState::Suspended);
    first.try_send(2).unwrap();
    assert_eq!(receiver.thread.state, ThreadState::Ready);
    assert_eq!(first.try_receive(), Ok(2));
    assert_eq!(second.try_receive(), Ok(1));
}

pub fn invalid_blocking_context_leaves_messages_and_waiters_unchanged() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut thread = cfs_thread("current", 1);
    let _reset = SchedulerReset::new();
    assert_eq!(queue.receive(), Err(MessageQueueStatus::NoCurrentThread));
    queue.send(1).unwrap();
    assert_eq!(queue.send(2), Err(MessageQueueStatus::NoCurrentThread));
    unsafe { start_cfs(&mut thread) };
    assert_eq!(queue.send(2), Err(MessageQueueStatus::InvalidContext));
    assert_eq!(queue.try_receive(), Ok(1));
    assert_eq!(queue.receive(), Err(MessageQueueStatus::InvalidContext));
    assert_eq!(thread.thread.state, ThreadState::Running);
    assert!(queue.is_empty());
    unsafe {
        assert!((*queue.state.get()).senders.is_empty());
        assert!((*queue.state.get()).receivers.is_empty());
    }
}

pub fn mixed_cfs_and_rt_waiters_share_deadline_ordering() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 2>::new();
    let mut background = cfs_thread("background", 1);
    let mut realtime = rt_thread("realtime");
    let mut timer = RtKTimer::new(20, ptr::null_mut(), "realtime");
    let _reset = SchedulerReset::new();
    let cfs_handle = unsafe { start_cfs(&mut background) };
    critical_section(|| queue.block(SyncType::MessageQueueReceive, true)).unwrap();
    unsafe {
        timer.init_rt_ktimer(&mut realtime.thread);
        enqueue_ktimer(timer.entity_mut());
        resume_rt(&mut realtime);
    }
    critical_section(|| queue.block(SyncType::MessageQueueReceive, true)).unwrap();

    queue.try_send(1).unwrap();
    assert_eq!(realtime.thread.state, ThreadState::Ready);
    assert_eq!(background.thread.state, ThreadState::Suspended);
    with_ktimer_queue(|timers| assert!(timers.contains(timer.entity_mut())));
    queue.try_send(2).unwrap();
    assert_eq!(background.thread.state, ThreadState::Ready);
    unsafe { assert!((*CFS_RUN_QUEUE.get()).contains(cfs_sched_entity(cfs_handle))) };
    assert_eq!(queue.try_receive(), Ok(1));
    assert_eq!(queue.try_receive(), Ok(2));
}
