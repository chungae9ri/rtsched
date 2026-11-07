// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Bounded, allocation-free message queues for scheduler threads.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr;

use crate::arch::platform::{blocking_context_available, request_context_switch};
use crate::clock::ticks_per_ms;
use crate::critical_section;
use crate::ktimer::current_time_ticks;
use crate::sync::{
    MessageWaitResult, SyncType, WaitTree, block_current_thread_until, current_thread_handle,
    finish_message_wait, insert_message_waiter, message_wait_tree, take_message_wait_result,
    wake_waiter,
};
use crate::thread::{ThreadHandle, ThreadState, thread_handle_from_sync_entity};
use crate::waitq::{WaitQueueError, wait_entity};

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
    /// A timed operation could not complete before its wait deadline.
    TimedOut,
    /// Another caller cancelled this operation while it was blocked.
    Cancelled,
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
/// Messages are copied into inline storage without heap allocation or payload
/// pointers. `N` must be nonzero. Sharing a queue between threads additionally
/// requires `T: Send`.
///
/// `send` and `receive` suspend a running thread indefinitely when the queue is
/// full or empty. Each successful operation wakes at most one unexpired opposite
/// waiter, selected by earliest captured scheduler deadline (equal deadlines use
/// thread storage address). Expired waiters encountered first are timed out and
/// woken too. A woken thread retries: messages and slots are not reserved,
/// so a competing operation can use them before it runs.
///
/// `try_send` and `try_receive` never suspend and may be called before scheduler
/// startup or from ordinary maskable interrupt handlers on the scheduler core.
/// NMI, HardFault, and multicore access are unsupported by the interrupt mask.
/// Keep messages small: the payload copy runs with interrupts masked.
///
/// `send_timeout` and `receive_timeout` bound the wait in milliseconds.
/// `cancel_wait` cancels a particular thread's blocked operation on this queue.
/// There are no close or overwrite operations. Keep a shared queue in static
/// storage for the lifetime of its users. Blocking calls must run with PendSV
/// enabled, outside any caller-owned critical section.
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
/// assert_eq!(EVENTS.receive_timeout(0), Err(MessageQueueStatus::TimedOut));
/// EVENTS.send_timeout(43, 0).unwrap();
/// assert_eq!(EVENTS.receive_timeout(0), Ok(43));
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
            {
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
            }

            // Release the mutable storage borrow before wakeup trace callbacks
            // can inspect or use this queue again.
            self.wake_one(false);
            Ok(())
        })
    }

    /// Remove the oldest message, or return `Empty` without changing the queue.
    ///
    /// On success, wake the earliest-deadline sender, if any.
    pub fn try_receive(&self) -> Result<T, MessageQueueStatus> {
        critical_section(|| unsafe {
            let message = {
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
                message
            };

            self.wake_one(true);
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

    /// Send a message, waiting at most `timeout_ms` milliseconds for space.
    ///
    /// The timeout starts at entry and is not restarted after a wakeup. An
    /// immediately available slot succeeds even with a zero timeout; otherwise
    /// zero returns `TimedOut` without blocking. After waiting, an expired
    /// deadline returns `TimedOut` without enqueueing. Clock configuration must
    /// be initialized with `update_sys_clk_freq` before using finite waits.
    pub fn send_timeout(&self, message: T, timeout_ms: u32) -> Result<(), MessageQueueStatus> {
        self.send_with_deadline(
            message,
            Some(deadline_after(timeout_ms)),
            blocking_context_available(),
            resume_after_block,
        )
    }

    /// Receive a message, waiting at most `timeout_ms` milliseconds for data.
    ///
    /// Uses the same deadline and zero-timeout rules as `send_timeout`. Timeout
    /// or cancellation never consumes a message. Expiration makes the thread
    /// ready; returning still depends on when the scheduler runs it.
    pub fn receive_timeout(&self, timeout_ms: u32) -> Result<T, MessageQueueStatus> {
        self.receive_with_deadline(
            Some(deadline_after(timeout_ms)),
            blocking_context_available(),
            resume_after_block,
        )
    }

    /// Cancel `thread`'s currently blocked send or receive on this queue.
    ///
    /// Returns `true` when cancellation wins and makes the thread ready; its
    /// operation returns `Cancelled` when it resumes. Returns `false` for a
    /// different queue, an idle/runnable thread, or an already completed wait.
    /// A wait whose deadline is already due is completed as `TimedOut` instead
    /// and returns `false`. Notification and cancellation are serialized: a
    /// receiver/sender already woken by its peer is no longer cancellable.
    ///
    /// This does not remove queued messages, affect future operations, or
    /// terminate the thread. Like the nonblocking messaging API, it may be used
    /// by ordinary maskable interrupt handlers on the scheduler core.
    pub fn cancel_wait(&self, thread: ThreadHandle) -> bool {
        critical_section(|| unsafe {
            if (*thread.as_ptr()).is_idle() || !(*thread.as_ptr()).state.is_blocked() {
                return false;
            }
            let tree = message_wait_tree(thread);
            let state = self.state.get();
            if tree.is_null()
                || (tree != ptr::addr_of_mut!((*state).senders)
                    && tree != ptr::addr_of_mut!((*state).receivers))
            {
                return false;
            }
            let result = wait_result(thread);
            let cancelled = result != MessageWaitResult::TimedOut;
            finish_message_wait(
                thread,
                if cancelled {
                    MessageWaitResult::Cancelled
                } else {
                    result
                },
            );
            Self::wake(thread);
            cancelled
        })
    }

    fn send_with_wait(
        &self,
        message: T,
        can_block: bool,
        wait: impl FnMut(),
    ) -> Result<(), MessageQueueStatus> {
        self.send_with_deadline(message, None, can_block, wait)
    }

    fn send_with_deadline(
        &self,
        message: T,
        deadline: Option<u64>,
        can_block: bool,
        mut wait: impl FnMut(),
    ) -> Result<(), MessageQueueStatus> {
        let mut waiter = None;
        loop {
            let sent = critical_section(|| {
                check_wait_result(waiter.take(), deadline)?;
                match self.try_send(message) {
                    Ok(()) => return Ok(true),
                    Err(MessageQueueStatus::Full) => {}
                    Err(error) => return Err(error),
                }
                // Check and registration share one critical section: a receive
                // cannot free space between them and leave this sender asleep.
                if deadline_expired(deadline) {
                    return Err(MessageQueueStatus::TimedOut);
                }
                waiter = Some(self.block_until(SyncType::MessageQueueSend, can_block, deadline)?);
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
        wait: impl FnMut(),
    ) -> Result<T, MessageQueueStatus> {
        self.receive_with_deadline(None, can_block, wait)
    }

    fn receive_with_deadline(
        &self,
        deadline: Option<u64>,
        can_block: bool,
        mut wait: impl FnMut(),
    ) -> Result<T, MessageQueueStatus> {
        let mut waiter = None;
        loop {
            let message = critical_section(|| {
                check_wait_result(waiter.take(), deadline)?;
                match self.try_receive() {
                    Ok(message) => return Ok(Some(message)),
                    Err(MessageQueueStatus::Empty) => {}
                    Err(error) => return Err(error),
                }
                if deadline_expired(deadline) {
                    return Err(MessageQueueStatus::TimedOut);
                }
                waiter =
                    Some(self.block_until(SyncType::MessageQueueReceive, can_block, deadline)?);
                Ok(None)
            })?;
            if let Some(message) = message {
                return Ok(message);
            }
            wait();
        }
    }

    // Caller holds the critical section and has just checked the ring condition.
    #[cfg(feature = "host")]
    fn block(&self, kind: SyncType, can_block: bool) -> Result<ThreadHandle, MessageQueueStatus> {
        self.block_until(kind, can_block, None)
    }

    fn block_until(
        &self,
        kind: SyncType,
        can_block: bool,
        deadline: Option<u64>,
    ) -> Result<ThreadHandle, MessageQueueStatus> {
        unsafe {
            let thread = current_thread_handle().ok_or(MessageQueueStatus::NoCurrentThread)?;
            if !can_block
                || (*thread.as_ptr()).state != ThreadState::Running
                || (*thread.as_ptr()).is_idle()
            {
                return Err(MessageQueueStatus::InvalidContext);
            }
            let deadline_at =
                block_current_thread_until(thread, kind, deadline.unwrap_or(u64::MAX))?;
            let available = {
                let state = &mut *self.state.get();
                let waiters = if kind == SyncType::MessageQueueSend {
                    &mut state.senders
                } else {
                    &mut state.receivers
                };
                insert_message_waiter(waiters, thread, deadline_at);
                if kind == SyncType::MessageQueueSend {
                    state.len < N
                } else {
                    state.len != 0
                }
            };
            // Scheduler accounting can invoke print/trace hooks. A hook may
            // use this queue nonblockingly between the initial check and waiter
            // insertion, even though interrupts remain masked throughout.
            let result = wait_result(thread);
            if available || result == MessageWaitResult::TimedOut {
                finish_message_wait(thread, result);
                Self::wake(thread);
            }
            request_context_switch();
            Ok(thread)
        }
    }

    // Caller holds the critical section. Expired waiters must be completed and
    // skipped even when their timer interrupt has not been dispatched yet.
    fn wake_one(&self, wake_senders: bool) {
        unsafe {
            loop {
                let state = self.state.get();
                let tree = if wake_senders {
                    ptr::addr_of_mut!((*state).senders)
                } else {
                    ptr::addr_of_mut!((*state).receivers)
                };
                let entity = (*tree).first();
                if entity.is_null() {
                    return;
                }
                let thread = thread_handle_from_sync_entity(entity);
                let result = wait_result(thread);
                finish_message_wait(thread, result);
                Self::wake(thread);
                if result == MessageWaitResult::Notified {
                    return;
                }
            }
        }
    }

    unsafe fn wake(thread: ThreadHandle) {
        // Registration already validated scheduler storage. A failure here
        // means an unsafe scheduler caller invalidated a registered waiter.
        unsafe { wake_waiter(thread) }.expect("message queue waiter must remain valid");
        request_context_switch();
    }
}

fn deadline_after(timeout_ms: u32) -> u64 {
    current_time_ticks()
        .saturating_add(u64::from(timeout_ms) * u64::from(ticks_per_ms()))
        .min(u64::MAX - 1)
}

fn deadline_expired(deadline: Option<u64>) -> bool {
    deadline.is_some_and(|deadline| current_time_ticks() >= deadline)
}

// Caller serializes scheduler state; u64::MAX denotes an indefinite wait.
unsafe fn wait_result(thread: ThreadHandle) -> MessageWaitResult {
    let wake_at = unsafe { (*wait_entity(thread)).wake_at };
    if wake_at != u64::MAX && current_time_ticks() >= wake_at {
        MessageWaitResult::TimedOut
    } else {
        MessageWaitResult::Notified
    }
}

fn check_wait_result(
    waiter: Option<ThreadHandle>,
    deadline: Option<u64>,
) -> Result<(), MessageQueueStatus> {
    if let Some(thread) = waiter {
        match unsafe { take_message_wait_result(thread) } {
            MessageWaitResult::TimedOut => return Err(MessageQueueStatus::TimedOut),
            MessageWaitResult::Cancelled => return Err(MessageQueueStatus::Cancelled),
            MessageWaitResult::Notified => {}
            MessageWaitResult::Pending => unreachable!("a pending message wait cannot resume"),
        }
        if deadline_expired(deadline) {
            return Err(MessageQueueStatus::TimedOut);
        }
    }
    Ok(())
}

impl<T: Copy, const N: usize> Default for MessageQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

fn resume_after_block() {
    // Make the pending PendSV take effect before retrying. This runs after
    // restoring the caller's interrupt mask, with no queue borrow held.
    #[cfg(all(target_arch = "arm", not(feature = "host")))]
    {
        cortex_m::asm::dsb();
        cortex_m::asm::isb();
    }
}

#[cfg(feature = "host")]
#[path = "../tests/support/messageq.rs"]
pub mod test_support;
