use rtsched::{MessageQueue, MessageQueueStatus, test_support::message};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

#[test]
fn fifo_wraparound_and_capacity_one() {
    let queue = MessageQueue::<u32, 3>::new();
    assert_eq!(queue.capacity(), 3);
    assert!(queue.is_empty());
    for value in 0..3 {
        queue.try_send(value).unwrap();
    }
    assert!(queue.is_full());
    assert_eq!(queue.try_send(99), Err(MessageQueueStatus::Full));
    assert_eq!(queue.try_receive(), Ok(0));
    queue.try_send(3).unwrap();
    for value in 1..4 {
        assert_eq!(queue.try_receive(), Ok(value));
    }
    assert_eq!(queue.try_receive(), Err(MessageQueueStatus::Empty));
    assert_eq!(queue.len(), 0);

    let single = MessageQueue::<&str, 1>::default();
    for _ in 0..100 {
        single.send("message").unwrap();
        assert_eq!(single.receive(), Ok("message"));
    }
}

#[test]
fn ring_matches_fifo_model_over_repeated_wraparound() {
    let queue = MessageQueue::<u32, 7>::new();
    let mut model = VecDeque::new();
    let mut random = 0x73b1_1234_u32;
    for value in 0..20_000 {
        random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        if random & 0x100 != 0 {
            let expected = if model.len() == 7 {
                Err(MessageQueueStatus::Full)
            } else {
                model.push_back(value);
                Ok(())
            };
            assert_eq!(queue.try_send(value), expected);
        } else {
            assert_eq!(
                queue.try_receive(),
                model.pop_front().ok_or(MessageQueueStatus::Empty)
            );
        }
        assert_eq!(queue.len(), model.len());
    }
}

#[test]
fn zero_sized_messages_still_observe_capacity() {
    let queue = MessageQueue::<(), 2>::new();
    assert_eq!(queue.try_send(()), Ok(()));
    assert_eq!(queue.try_send(()), Ok(()));
    assert_eq!(queue.try_send(()), Err(MessageQueueStatus::Full));
    assert_eq!(queue.try_receive(), Ok(()));
    assert_eq!(queue.try_receive(), Ok(()));
    assert_eq!(queue.try_receive(), Err(MessageQueueStatus::Empty));
}

#[test]
#[should_panic(expected = "message queue capacity must be non-zero")]
fn zero_capacity_is_rejected() {
    let _ = MessageQueue::<u32, 0>::new();
}

#[test]
fn queue_can_transfer_send_values_without_requiring_sync() {
    #[derive(Clone, Copy)]
    struct SendOnly(core::marker::PhantomData<core::cell::Cell<()>>);
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MessageQueue<SendOnly, 2>>();
}

#[test]
fn multiple_host_producers_and_consumers_deliver_each_message_once() {
    const PRODUCERS: usize = 4;
    const CONSUMERS: usize = 4;
    const PER_PRODUCER: usize = 2_000;
    const TOTAL: usize = PRODUCERS * PER_PRODUCER;
    let queue = Arc::new(MessageQueue::<usize, 7>::new());
    let barrier = Arc::new(Barrier::new(PRODUCERS + CONSUMERS));
    let received = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for producer in 0..PRODUCERS {
            let queue = Arc::clone(&queue);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                for value in producer * PER_PRODUCER..(producer + 1) * PER_PRODUCER {
                    loop {
                        match queue.try_send(value) {
                            Ok(()) => break,
                            Err(MessageQueueStatus::Full) => std::thread::yield_now(),
                            Err(error) => panic!("unexpected send error: {error:?}"),
                        }
                    }
                }
            });
        }
        let mut consumers = Vec::new();
        for _ in 0..CONSUMERS {
            let queue = Arc::clone(&queue);
            let barrier = Arc::clone(&barrier);
            let received = Arc::clone(&received);
            consumers.push(scope.spawn(move || {
                let mut messages = Vec::new();
                barrier.wait();
                while received.load(Ordering::Relaxed) < TOTAL {
                    match queue.try_receive() {
                        Ok(value) => {
                            messages.push(value);
                            received.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(MessageQueueStatus::Empty) => std::thread::yield_now(),
                        Err(error) => panic!("unexpected receive error: {error:?}"),
                    }
                }
                messages
            }));
        }
        let mut messages: Vec<_> = consumers
            .into_iter()
            .flat_map(|consumer| consumer.join().unwrap())
            .collect();
        messages.sort_unstable();
        assert_eq!(messages, (0..TOTAL).collect::<Vec<_>>());
    });
    assert!(queue.is_empty());
}

#[test]
fn cfs_receiver_suspends_and_resumes_with_message() {
    message::cfs_receiver_suspends_and_resumes_with_message();
}

#[test]
fn cfs_sender_suspends_until_space_is_available() {
    message::cfs_sender_suspends_until_space_is_available();
}

#[test]
fn rt_receiver_retries_after_another_consumer_takes_the_message() {
    message::rt_receiver_retries_after_another_consumer_takes_the_message();
}

#[test]
fn rt_sender_retries_after_another_producer_fills_the_slot() {
    message::rt_sender_retries_after_another_producer_fills_the_slot();
}

#[test]
fn queue_waiters_wake_by_deadline_and_address_in_both_directions() {
    message::queue_waiters_wake_by_deadline_and_address_in_both_directions();
}

#[test]
fn queues_do_not_wake_each_others_waiters() {
    message::queues_do_not_wake_each_others_waiters();
}

#[test]
fn mixed_cfs_and_rt_waiters_share_deadline_ordering() {
    message::mixed_cfs_and_rt_waiters_share_deadline_ordering();
}

#[test]
fn invalid_blocking_context_leaves_messages_and_waiters_unchanged() {
    message::invalid_blocking_context_leaves_messages_and_waiters_unchanged();
}

#[test]
fn message_queue_event_tags_round_trip() {
    for (kind, tag) in [
        (rtsched::SyncType::MessageQueueSend, *b"mqtx"),
        (rtsched::SyncType::MessageQueueReceive, *b"mqrx"),
    ] {
        assert_eq!(kind.wait_event(), u32::from_be_bytes(tag));
        assert_eq!(
            rtsched::SyncType::from_wait_event(kind.wait_event()),
            Some(kind)
        );
    }
}
