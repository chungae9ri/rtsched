// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Bounded, allocation-free message queues for scheduler threads.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;

use crate::arch::platform::{blocking_context_available, request_context_switch};
use crate::critical_section;
use crate::sync::{
    SyncType, WaitTree, block_current_thread, current_thread_handle, insert_sync_waiter,
    pop_waiting_thread, wake_waiter,
};
use crate::thread::ThreadState;
use crate::waitq::WaitQueueError;

/// Non-success status returned by a message queue operation.
///
/// `Full` and `Empty` are normal, retryable outcomes of nonblocking operations.
/// Successful operations return `Ok`; these statuses are carried by `Err`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageQueueStatus {
    /// A nonblocking send found no free slot.
    Full,
    /// A nonblocking receive found no message.
    Empty,
    /// Blocking would be necessary, but no scheduler thread is selected.
    NoCurrentThread,
    /// Blocking would be necessary outside a running, non-idle Cortex-M thread,
    /// or while exceptions are masked. Host builds cannot context-switch.
    InvalidContext,
    /// The scheduler could not move the current thread to its wait queue.
    WaitQueue(WaitQueueError),
}

impl From<WaitQueueError> for MessageQueueStatus {
    fn from(error: WaitQueueError) -> Self {
        Self::WaitQueue(error)
    }
}

struct QueueState<T: Copy, const N: usize> {
    slots: [MaybeUninit<T>; N],
    head: usize,
    len: usize,
    senders: WaitTree,
    receivers: WaitTree,
}

/// A fixed-capacity FIFO message queue with multiple producers and consumers.
///
/// Messages are copied into inline storage; no heap, payload pointers, or extra
/// per-thread fields are needed. `N` must be nonzero. Sharing a queue between
/// threads additionally requires `T: Send`.
///
/// `send` and `receive` suspend a running thread indefinitely when the queue is
/// full or empty. Each successful operation wakes at most one opposite waiter,
/// selected by earliest captured scheduler deadline (equal deadlines use thread
/// storage address). A woken thread retries: messages and slots are not reserved,
/// so a competing operation can use them before it runs.
///
/// `try_send` and `try_receive` never suspend and may be called before scheduler
/// startup or from ordinary maskable interrupt handlers on the scheduler core.
/// NMI, HardFault, and multicore access are unsupported by the interrupt mask.
/// Keep messages small: the payload copy runs with interrupts masked.
///
/// There are no timeouts, cancellation, close, or overwrite operations. Keep a
/// shared queue in static storage for the lifetime of its users. Blocking calls
/// must run with PendSV enabled, outside any caller-owned critical section.
/// Native host builds support nonblocking operations; a blocking call that
/// cannot complete immediately returns an error instead of parking an OS thread.
///
/// ```
/// use rtsched::{MessageQueue, MessageQueueStatus};
///
/// static EVENTS: MessageQueue<u32, 8> = MessageQueue::new();
/// assert_eq!(EVENTS.try_receive(), Err(MessageQueueStatus::Empty));
/// EVENTS.try_send(42).unwrap();
/// assert_eq!(EVENTS.try_receive(), Ok(42));
/// ```
///
/// A queue cannot transfer a non-`Send` payload across threads:
///
/// ```compile_fail
/// use rtsched::MessageQueue;
/// static POINTERS: MessageQueue<*const u8, 1> = MessageQueue::new();
/// ```
pub struct MessageQueue<T: Copy, const N: usize> {
    state: UnsafeCell<QueueState<T, N>>,
}

// Every storage and waiter access is serialized by the platform critical
// section. Sending copied values across threads requires T: Send, not T: Sync.
unsafe impl<T: Copy + Send, const N: usize> Sync for MessageQueue<T, N> {}
unsafe impl<T: Copy + Send, const N: usize> Send for MessageQueue<T, N> {}

impl<T: Copy, const N: usize> MessageQueue<T, N> {
    /// Create an empty queue. Panics if `N == 0` (also checked in const contexts).
    pub const fn new() -> Self {
        assert!(N != 0, "message queue capacity must be non-zero");
        Self {
            state: UnsafeCell::new(QueueState {
                slots: [MaybeUninit::uninit(); N],
                head: 0,
                len: 0,
                senders: WaitTree::new(),
                receivers: WaitTree::new(),
            }),
        }
    }

    /// Return the compile-time capacity in messages.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Return a snapshot of the number of buffered messages.
    ///
    /// Another thread or interrupt can change this immediately after return.
    pub fn len(&self) -> usize {
        critical_section(|| unsafe { (*self.state.get()).len })
    }

    /// Return whether the queue is empty at the instant of inspection.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return whether the queue is full at the instant of inspection.
    pub fn is_full(&self) -> bool {
        self.len() == N
    }

    /// Copy a message into the queue, or return `Full` without changing it.
    ///
    /// On success, wake the earliest-deadline receiver, if any. The caller
    /// retains its original value because `T: Copy`.
    pub fn try_send(&self, message: T) -> Result<(), MessageQueueStatus> {
        critical_section(|| unsafe {
            let waiter = {
                let state = &mut *self.state.get();
                if state.len == N {
                    return Err(MessageQueueStatus::Full);
                }

                // Branch rather than divide; neither head + len nor the
                // successor computation needs to overflow even for huge N.
                let until_end = N - state.head;
                let tail = if state.len < until_end {
                    state.head + state.len
                } else {
                    state.len - until_end
                };
                state.slots[tail].write(message);
                state.len += 1;
                pop_waiting_thread(&mut state.receivers)
            };

            // Release the mutable storage borrow before wakeup trace callbacks
            // can inspect or use this queue again.
            Self::wake(waiter);
            Ok(())
        })
    }

    /// Remove the oldest message, or return `Empty` without changing the queue.
    ///
    /// On success, wake the earliest-deadline sender, if any.
    pub fn try_receive(&self) -> Result<T, MessageQueueStatus> {
        critical_section(|| unsafe {
            let (message, waiter) = {
                let state = &mut *self.state.get();
                if state.len == 0 {
                    return Err(MessageQueueStatus::Empty);
                }

                // Only the len initialized slots starting at head are readable.
                let message = state.slots[state.head].assume_init_read();
                state.head = if state.head == N - 1 {
                    0
                } else {
                    state.head + 1
                };
                state.len -= 1;
                (message, pop_waiting_thread(&mut state.senders))
            };

            Self::wake(waiter);
            Ok(message)
        })
    }

    /// Send a message, suspending and retrying while the queue is full.
    ///
    /// Blocking is indefinite and does not reserve the next free slot. On any
    /// returned error this call has not enqueued its message.
    pub fn send(&self, message: T) -> Result<(), MessageQueueStatus> {
        self.send_with_wait(message, blocking_context_available(), resume_after_block)
    }

    /// Receive the oldest message, suspending and retrying while empty.
    ///
    /// Blocking is indefinite and does not reserve the next arriving message.
    /// On any returned error this call has not removed a message.
    pub fn receive(&self) -> Result<T, MessageQueueStatus> {
        self.receive_with_wait(blocking_context_available(), resume_after_block)
    }

    fn send_with_wait(
        &self,
        message: T,
        can_block: bool,
        mut wait: impl FnMut(),
    ) -> Result<(), MessageQueueStatus> {
        loop {
            let sent = critical_section(|| {
                match self.try_send(message) {
                    Ok(()) => return Ok(true),
                    Err(MessageQueueStatus::Full) => {}
                    Err(error) => return Err(error),
                }
                // Check and registration share one critical section: a receive
                // cannot free space between them and leave this sender asleep.
                self.block(SyncType::MessageQueueSend, can_block)?;
                Ok(false)
            })?;
            if sent {
                return Ok(());
            }
            wait();
        }
    }

    fn receive_with_wait(
        &self,
        can_block: bool,
        mut wait: impl FnMut(),
    ) -> Result<T, MessageQueueStatus> {
        loop {
            let message = critical_section(|| {
                match self.try_receive() {
                    Ok(message) => return Ok(Some(message)),
                    Err(MessageQueueStatus::Empty) => {}
                    Err(error) => return Err(error),
                }
                self.block(SyncType::MessageQueueReceive, can_block)?;
                Ok(None)
            })?;
            if let Some(message) = message {
                return Ok(message);
            }
            wait();
        }
    }

    // Caller holds the critical section and has just checked the ring condition.
    fn block(&self, kind: SyncType, can_block: bool) -> Result<(), MessageQueueStatus> {
        unsafe {
            let thread = current_thread_handle().ok_or(MessageQueueStatus::NoCurrentThread)?;
            if !can_block
                || (*thread.as_ptr()).state != ThreadState::Running
                || (*thread.as_ptr()).is_idle()
            {
                return Err(MessageQueueStatus::InvalidContext);
            }
            let deadline_at = block_current_thread(thread, kind)?;
            let state = &mut *self.state.get();
            let waiters = if kind == SyncType::MessageQueueSend {
                &mut state.senders
            } else {
                &mut state.receivers
            };
            insert_sync_waiter(waiters, thread, deadline_at);
            request_context_switch();
            Ok(())
        }
    }

    unsafe fn wake(waiter: Option<crate::thread::ThreadHandle>) {
        if let Some(waiter) = waiter {
            // Registration already validated scheduler storage. A failure here
            // means an unsafe scheduler caller invalidated a registered waiter.
            unsafe { wake_waiter(waiter) }.expect("message queue waiter must remain valid");
            request_context_switch();
        }
    }
}

impl<T: Copy, const N: usize> Default for MessageQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

fn resume_after_block() {
    // Make the pending PendSV take effect before retrying. This runs after
    // restoring the caller's interrupt mask, with no queue borrow held.
    #[cfg(target_arch = "arm")]
    {
        cortex_m::asm::dsb();
        cortex_m::asm::isb();
    }
}

#[cfg(not(target_arch = "arm"))]
#[path = "../tests/support/message.rs"]
pub mod test_support;
