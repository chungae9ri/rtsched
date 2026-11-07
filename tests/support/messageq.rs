extern crate std;

use super::*;
use crate::ktimer::{
    RtKTimer, advance_ktimers, dispatch_expired_ktimer, enqueue_ktimer, init_ktimer_queue,
    update_next_ktimer, with_ktimer_queue,
};
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
            crate::clock::update_sys_clk_freq(1_000);
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
            WAIT_QUEUE.reset_for_test();
            init_ktimer_queue();
            init_cfs(1_000, 100);
        }
    }
}

fn advance_message_time(ticks: u32, dispatch: bool) {
    critical_section(|| unsafe {
        advance_ktimers(ticks);
        if dispatch {
            let next = dispatch_expired_ktimer(0);
            update_next_ktimer(next);
        }
    });
}

unsafe fn start_message_thread(
    is_rt: bool,
    cfs: &mut CfsThread,
    rt: &mut RtThread,
    timer: &mut RtKTimer,
) -> ThreadHandle {
    unsafe {
        if is_rt {
            timer.init_rt_ktimer(&mut rt.thread);
            enqueue_ktimer(timer.entity_mut());
            resume_rt(rt)
        } else {
            start_cfs(cfs)
        }
    }
}

unsafe fn resume_message_thread(is_rt: bool, cfs: &mut CfsThread, rt: &mut RtThread) {
    unsafe {
        if is_rt {
            resume_rt(rt);
        } else {
            resume_cfs(cfs);
        }
    }
}

fn run_message_wait(
    queue: &MessageQueue<u32, 1>,
    sending: bool,
    deadline: Option<u64>,
    wait: impl FnMut(),
) -> Result<Option<u32>, MessageQueueStatus> {
    if sending {
        queue
            .send_with_deadline(2, deadline, true, wait)
            .map(|()| None)
    } else {
        queue.receive_with_deadline(deadline, true, wait).map(Some)
    }
}

fn assert_message_wait_detached(queue: &MessageQueue<u32, 1>, thread: ThreadHandle) {
    unsafe {
        assert!((*queue.state.get()).senders.is_empty());
        assert!((*queue.state.get()).receivers.is_empty());
        assert!(message_wait_tree(thread).is_null());
        assert!((*WAIT_QUEUE.get()).is_empty());
        let mut cursor = WAIT_QUEUE.next_wait_entity(None);
        while !cursor.is_null() {
            assert_ne!(cursor, wait_entity(thread));
            let owner = crate::thread::thread_handle_from_wait_entity(cursor);
            cursor = WAIT_QUEUE.next_wait_entity(Some(owner));
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

pub fn timed_waits_expire_and_leave_both_wait_indexes() {
    let _lock = TEST_LOCK.lock().unwrap();
    for is_rt in [false, true] {
        for sending in [false, true] {
            let queue = MessageQueue::<u32, 1>::new();
            let mut cfs = cfs_thread("cfs", 1);
            let mut rt = rt_thread("rt");
            let mut timer = RtKTimer::new(100, ptr::null_mut(), "rt");
            let _reset = SchedulerReset::new();
            let handle = unsafe { start_message_thread(is_rt, &mut cfs, &mut rt, &mut timer) };
            if sending {
                queue.try_send(1).unwrap();
            }
            let mut waits = 0;
            let result = run_message_wait(&queue, sending, Some(deadline_after(10)), || {
                waits += 1;
                assert_eq!(handle.state(), ThreadState::Waiting);
                unsafe { assert_eq!((*wait_entity(handle)).wake_at, 10) };
                advance_message_time(9, true);
                assert_eq!(handle.state(), ThreadState::Waiting);
                advance_message_time(1, true);
                assert_eq!(handle.state(), ThreadState::Ready);
                assert_message_wait_detached(&queue, handle);
                // A resource made available after expiry must not turn timeout
                // into success or cause the failed operation to consume it.
                if sending {
                    assert_eq!(queue.try_receive(), Ok(1));
                } else {
                    queue.try_send(9).unwrap();
                }
                unsafe { resume_message_thread(is_rt, &mut cfs, &mut rt) };
            });
            assert_eq!(result, Err(MessageQueueStatus::TimedOut));
            assert_eq!(waits, 1);
            assert_eq!(
                queue.try_receive(),
                if sending {
                    Err(MessageQueueStatus::Empty)
                } else {
                    Ok(9)
                }
            );
            assert!(!queue.cancel_wait(handle));
        }
    }
}

pub fn timed_waits_succeed_before_deadline() {
    let _lock = TEST_LOCK.lock().unwrap();
    for is_rt in [false, true] {
        for sending in [false, true] {
            let queue = MessageQueue::<u32, 1>::new();
            let mut cfs = cfs_thread("cfs", 1);
            let mut rt = rt_thread("rt");
            let mut timer = RtKTimer::new(100, ptr::null_mut(), "rt");
            let _reset = SchedulerReset::new();
            let handle = unsafe { start_message_thread(is_rt, &mut cfs, &mut rt, &mut timer) };
            if sending {
                queue.try_send(1).unwrap();
            }
            let result = run_message_wait(&queue, sending, Some(deadline_after(20)), || {
                advance_message_time(5, true);
                if sending {
                    assert_eq!(queue.try_receive(), Ok(1));
                } else {
                    queue.try_send(9).unwrap();
                }
                assert_eq!(handle.state(), ThreadState::Ready);
                assert_message_wait_detached(&queue, handle);
                assert!(!queue.cancel_wait(handle));
                unsafe { resume_message_thread(is_rt, &mut cfs, &mut rt) };
            });
            assert_eq!(result, Ok(if sending { None } else { Some(9) }));
            if sending {
                assert_eq!(queue.try_receive(), Ok(2));
            }
            // Its former timeout must not wake/reinsert this thread again.
            advance_message_time(20, true);
            assert_eq!(handle.state(), ThreadState::Running);
            assert_message_wait_detached(&queue, handle);
        }
    }
}

pub fn timeout_deadlines_survive_reblocking() {
    let _lock = TEST_LOCK.lock().unwrap();
    for is_rt in [false, true] {
        for sending in [false, true] {
            let queue = MessageQueue::<u32, 1>::new();
            let mut cfs = cfs_thread("cfs", 1);
            let mut rt = rt_thread("rt");
            let mut timer = RtKTimer::new(100, ptr::null_mut(), "rt");
            let _reset = SchedulerReset::new();
            let handle = unsafe { start_message_thread(is_rt, &mut cfs, &mut rt, &mut timer) };
            if sending {
                queue.try_send(1).unwrap();
            }
            let mut waits = 0;
            let result = run_message_wait(&queue, sending, Some(deadline_after(10)), || {
                waits += 1;
                assert!(waits <= 2);
                unsafe { assert_eq!((*wait_entity(handle)).wake_at, 10) };
                if waits == 1 {
                    advance_message_time(4, true);
                    if sending {
                        assert_eq!(queue.try_receive(), Ok(1));
                        queue.try_send(3).unwrap();
                    } else {
                        queue.try_send(1).unwrap();
                        assert_eq!(queue.try_receive(), Ok(1));
                    }
                } else {
                    advance_message_time(6, true);
                    assert_message_wait_detached(&queue, handle);
                }
                unsafe { resume_message_thread(is_rt, &mut cfs, &mut rt) };
            });
            assert_eq!(waits, 2);
            assert_eq!(result, Err(MessageQueueStatus::TimedOut));
            assert_eq!(
                queue.try_receive(),
                if sending {
                    Ok(3)
                } else {
                    Err(MessageQueueStatus::Empty)
                }
            );
        }
    }
}

pub fn cancellation_cleans_up_cfs_and_rt_operations() {
    let _lock = TEST_LOCK.lock().unwrap();
    for is_rt in [false, true] {
        for sending in [false, true] {
            for timed in [false, true] {
                let queue = MessageQueue::<u32, 1>::new();
                let other = MessageQueue::<u32, 1>::new();
                let mut cfs = cfs_thread("cfs", 1);
                let mut rt = rt_thread("rt");
                let mut timer = RtKTimer::new(100, ptr::null_mut(), "rt");
                let _reset = SchedulerReset::new();
                let handle = unsafe { start_message_thread(is_rt, &mut cfs, &mut rt, &mut timer) };
                assert!(!queue.cancel_wait(handle));
                if sending {
                    queue.try_send(1).unwrap();
                }
                let deadline = timed.then(|| deadline_after(5));
                let result = run_message_wait(&queue, sending, deadline, || {
                    assert_eq!(
                        handle.state(),
                        if timed {
                            ThreadState::Waiting
                        } else {
                            ThreadState::Suspended
                        }
                    );
                    assert!(!other.cancel_wait(handle));
                    assert!(queue.cancel_wait(handle));
                    assert_eq!(handle.state(), ThreadState::Ready);
                    assert!(!queue.cancel_wait(handle));
                    assert_message_wait_detached(&queue, handle);
                    // Cancellation remains the result even if its old deadline
                    // passes and data/space becomes available before it runs.
                    advance_message_time(6, true);
                    if sending {
                        assert_eq!(queue.try_receive(), Ok(1));
                    } else {
                        queue.try_send(9).unwrap();
                    }
                    unsafe { resume_message_thread(is_rt, &mut cfs, &mut rt) };
                });
                assert_eq!(result, Err(MessageQueueStatus::Cancelled));
                assert_eq!(
                    queue.try_receive(),
                    if sending {
                        Err(MessageQueueStatus::Empty)
                    } else {
                        Ok(9)
                    }
                );
                // A new operation on the same thread is not cancelled.
                let next = queue.receive_with_wait(true, || {
                    queue.try_send(7).unwrap();
                    unsafe { resume_message_thread(is_rt, &mut cfs, &mut rt) };
                });
                assert_eq!(next, Ok(7));
                assert_message_wait_detached(&queue, handle);
            }
        }
    }
}

pub fn expired_waiters_do_not_steal_peer_wakeups() {
    let _lock = TEST_LOCK.lock().unwrap();
    for sending in [false, true] {
        let queue = MessageQueue::<u32, 3>::new();
        let mut threads = [rt_thread("expired"), rt_thread("live"), rt_thread("last")];
        let mut timers = [
            RtKTimer::new(20, ptr::null_mut(), "expired"),
            RtKTimer::new(30, ptr::null_mut(), "live"),
            RtKTimer::new(40, ptr::null_mut(), "last"),
        ];
        let _reset = SchedulerReset::new();
        if sending {
            for message in 0..3 {
                queue.try_send(message).unwrap();
            }
        }
        let kind = if sending {
            SyncType::MessageQueueSend
        } else {
            SyncType::MessageQueueReceive
        };
        let mut handles = std::vec::Vec::new();
        for (index, (thread, timer)) in threads.iter_mut().zip(&mut timers).enumerate() {
            unsafe {
                timer.init_rt_ktimer(&mut thread.thread);
                enqueue_ktimer(timer.entity_mut());
                handles.push(resume_rt(thread));
            }
            critical_section(|| {
                queue.block_until(kind, true, if index == 0 { Some(5) } else { None })
            })
            .unwrap();
        }
        // Leave the timer interrupt pending; a peer must detect the expiry itself.
        advance_message_time(5, false);
        if sending {
            assert_eq!(queue.try_receive(), Ok(0));
        } else {
            queue.try_send(9).unwrap();
        }
        assert_eq!(handles[0].state(), ThreadState::Ready);
        assert_eq!(handles[1].state(), ThreadState::Ready);
        assert_eq!(handles[2].state(), ThreadState::Suspended);
        unsafe {
            assert_eq!(
                take_message_wait_result(handles[0]),
                MessageWaitResult::TimedOut
            );
            assert_eq!(
                take_message_wait_result(handles[1]),
                MessageWaitResult::Notified
            );
        }
        assert!(queue.cancel_wait(handles[2]));
        unsafe {
            assert!((*queue.state.get()).senders.is_empty());
            assert!((*queue.state.get()).receivers.is_empty());
            assert!(WAIT_QUEUE.next_wait_entity(None).is_null());
        }
    }
}

pub fn timeout_wins_cancellation_at_deadline() {
    let _lock = TEST_LOCK.lock().unwrap();
    for is_rt in [false, true] {
        for sending in [false, true] {
            let queue = MessageQueue::<u32, 1>::new();
            let mut cfs = cfs_thread("cfs", 1);
            let mut rt = rt_thread("rt");
            let mut timer = RtKTimer::new(100, ptr::null_mut(), "rt");
            let _reset = SchedulerReset::new();
            let handle = unsafe { start_message_thread(is_rt, &mut cfs, &mut rt, &mut timer) };
            if sending {
                queue.try_send(1).unwrap();
            }
            let result = run_message_wait(&queue, sending, Some(5), || {
                advance_message_time(5, false);
                assert!(!queue.cancel_wait(handle));
                assert_eq!(handle.state(), ThreadState::Ready);
                assert_message_wait_detached(&queue, handle);
                advance_message_time(1, true);
                unsafe { resume_message_thread(is_rt, &mut cfs, &mut rt) };
            });
            assert_eq!(result, Err(MessageQueueStatus::TimedOut));
        }
    }
}

pub fn long_timeouts_keep_u64_deadlines() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut receiver = cfs_thread("receiver", 1);
    let _reset = SchedulerReset::new();
    crate::clock::update_sys_clk_freq(12_000_000);
    let handle = unsafe { start_cfs(&mut receiver) };
    let deadline = deadline_after(u32::MAX);
    assert_eq!(deadline, u64::from(u32::MAX) * 12_000);
    let result = queue.receive_with_deadline(Some(deadline), true, || {
        unsafe { assert_eq!((*wait_entity(handle)).wake_at, deadline) };
        assert!(queue.cancel_wait(handle));
        unsafe { resume_cfs(&mut receiver) };
    });
    assert_eq!(result, Err(MessageQueueStatus::Cancelled));
    assert_message_wait_detached(&queue, handle);
}

pub fn cancellation_ignores_non_message_waits_and_idle_threads() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut thread = cfs_thread("semaphore", 1);
    let mut semaphore_waiters = WaitTree::new();
    let mut idle = crate::test_support::trace_thread(9, "idle", false);
    idle.kind = crate::thread::ThreadKind::Idle;
    let _reset = SchedulerReset::new();
    let handle = unsafe { start_cfs(&mut thread) };
    critical_section(|| unsafe {
        let deadline =
            crate::sync::block_current_thread(handle, SyncType::BinarySemaphore).unwrap();
        crate::sync::insert_sync_waiter(&mut semaphore_waiters, handle, deadline);
    });
    assert!(!queue.cancel_wait(handle));
    assert_eq!(handle.state(), ThreadState::Suspended);
    critical_section(|| unsafe {
        assert_eq!(
            crate::sync::pop_waiting_thread(&mut semaphore_waiters),
            Some(handle)
        );
        wake_waiter(handle).unwrap();
        let idle_handle = ThreadHandle::from_thread_ctx(&mut idle);
        assert!(!queue.cancel_wait(idle_handle));
    });
}

pub fn timed_wait_context_errors_do_not_register_waiters() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut thread = cfs_thread("current", 1);
    let _reset = SchedulerReset::new();
    assert_eq!(
        queue.receive_timeout(5),
        Err(MessageQueueStatus::NoCurrentThread)
    );
    queue.try_send(1).unwrap();
    assert_eq!(
        queue.send_timeout(2, 5),
        Err(MessageQueueStatus::NoCurrentThread)
    );
    let handle = unsafe { start_cfs(&mut thread) };
    assert_eq!(
        queue.send_timeout(2, 5),
        Err(MessageQueueStatus::InvalidContext)
    );
    assert_eq!(queue.try_receive(), Ok(1));
    assert_eq!(
        queue.receive_timeout(5),
        Err(MessageQueueStatus::InvalidContext)
    );
    assert_eq!(handle.state(), ThreadState::Running);
    assert_message_wait_detached(&queue, handle);
}

pub fn notified_wait_can_timeout_before_dispatch() {
    let _lock = TEST_LOCK.lock().unwrap();
    for is_rt in [false, true] {
        for sending in [false, true] {
            let queue = MessageQueue::<u32, 1>::new();
            let mut cfs = cfs_thread("cfs", 1);
            let mut rt = rt_thread("rt");
            let mut timer = RtKTimer::new(100, ptr::null_mut(), "rt");
            let _reset = SchedulerReset::new();
            let handle = unsafe { start_message_thread(is_rt, &mut cfs, &mut rt, &mut timer) };
            if sending {
                queue.try_send(1).unwrap();
            }
            let result = run_message_wait(&queue, sending, Some(10), || {
                advance_message_time(4, true);
                if sending {
                    assert_eq!(queue.try_receive(), Ok(1));
                } else {
                    queue.try_send(9).unwrap();
                }
                assert_eq!(handle.state(), ThreadState::Ready);
                assert!(!queue.cancel_wait(handle));
                advance_message_time(6, true);
                unsafe { resume_message_thread(is_rt, &mut cfs, &mut rt) };
            });
            assert_eq!(result, Err(MessageQueueStatus::TimedOut));
            assert_eq!(
                queue.try_receive(),
                if sending {
                    Err(MessageQueueStatus::Empty)
                } else {
                    Ok(9)
                }
            );
            assert_message_wait_detached(&queue, handle);
        }
    }
}

pub fn staggered_timeouts_and_cancellation_preserve_remaining_waiters() {
    let _lock = TEST_LOCK.lock().unwrap();
    let queue = MessageQueue::<u32, 1>::new();
    let mut threads = [rt_thread("first"), rt_thread("middle"), rt_thread("last")];
    let mut timers = [
        RtKTimer::new(100, ptr::null_mut(), "first"),
        RtKTimer::new(110, ptr::null_mut(), "middle"),
        RtKTimer::new(120, ptr::null_mut(), "last"),
    ];
    let _reset = SchedulerReset::new();
    let mut handles = std::vec::Vec::new();
    for (index, (thread, timer)) in threads.iter_mut().zip(&mut timers).enumerate() {
        unsafe {
            timer.init_rt_ktimer(&mut thread.thread);
            enqueue_ktimer(timer.entity_mut());
            handles.push(resume_rt(thread));
        }
        critical_section(|| {
            queue.block_until(
                SyncType::MessageQueueReceive,
                true,
                Some((index as u64 + 1) * 5),
            )
        })
        .unwrap();
    }
    assert!(queue.cancel_wait(handles[1]));
    unsafe { assert_eq!((*WAIT_QUEUE.get()).len(), 2) };
    advance_message_time(5, true);
    assert_eq!(handles[0].state(), ThreadState::Ready);
    assert_eq!(handles[2].state(), ThreadState::Waiting);
    unsafe {
        assert_eq!(
            take_message_wait_result(handles[0]),
            MessageWaitResult::TimedOut
        );
        assert_eq!(
            take_message_wait_result(handles[1]),
            MessageWaitResult::Cancelled
        );
        assert_eq!((*WAIT_QUEUE.get()).len(), 1);
    }
    advance_message_time(9, true);
    assert_eq!(handles[2].state(), ThreadState::Waiting);
    advance_message_time(1, true);
    assert_eq!(handles[2].state(), ThreadState::Ready);
    unsafe {
        assert_eq!(
            take_message_wait_result(handles[2]),
            MessageWaitResult::TimedOut
        )
    };
    assert_message_wait_detached(&queue, handles[2]);
    // Subsequent queue operations cannot encounter any of the old wait nodes.
    queue.try_send(9).unwrap();
    assert_eq!(queue.try_receive(), Ok(9));
}

pub fn reentrant_publish_during_block_registration_is_not_lost() {
    static QUEUE: MessageQueue<u32, 1> = MessageQueue::new();
    fn publish_from_print(_message: &str) {
        let _ = QUEUE.try_send(9);
    }
    struct ClearPrint;
    impl Drop for ClearPrint {
        fn drop(&mut self) {
            crate::print::clear_print_fn_for_test();
        }
    }
    let _lock = TEST_LOCK.lock().unwrap();
    let mut receiver = rt_thread("receiver");
    let mut timer = RtKTimer::new_with_timing(
        crate::ktimer::RtTiming::new(200, 200, 10),
        ptr::null_mut(),
        "receiver",
    );
    let _reset = SchedulerReset::new();
    let _clear_print = ClearPrint;
    while QUEUE.try_receive().is_ok() {}
    unsafe {
        timer.init_rt_ktimer(&mut receiver.thread);
        enqueue_ktimer(timer.entity_mut());
        resume_rt(&mut receiver);
    }
    // Blocking accounts runtime and prints a budget-overrun diagnostic. Its
    // callback posts after the empty check but before object-tree insertion.
    receiver.runtime = 11;
    crate::print::set_print_fn(publish_from_print);
    let value = QUEUE.receive_with_wait(true, || {
        assert_eq!(receiver.thread.state, ThreadState::Ready);
        unsafe { resume_rt(&mut receiver) };
    });
    assert_eq!(value, Ok(9));
    assert!(QUEUE.is_empty());
}
