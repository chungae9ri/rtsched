// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Synchronization primitives built on the scheduler wait queue.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::offset_of;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::platform::request_context_switch;
use crate::critical_section;
use crate::ktimer::{
    CFS_KTIMER, dequeue_ktimerq_to_waitq, earliest_queued_scheduler_deadline_at,
    elapsed_ticks_since_current_reload, enqueue_ktimerq_from_waitq, program_wait_ktimer,
    set_thread_scheduler_deadline_at, thread_scheduler_deadline_at, yield_ktimer,
};
use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::runq::{dequeue_runq_to_waitq, enqueue_runq_from_waitq};
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS};
use crate::thread::{
    ThreadHandle, ThreadState, rt_ktimer_entity, sync_entity, thread_handle_from_sync_entity,
};
use crate::waitq::{WaitQueueError, remove_wait_thread, wait_entity};

/// Synchronization primitive type encoded in a waiting thread's event tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum SyncType {
    BinarySemaphore = u32::from_be_bytes(*b"semb"),
    CountingSemaphore = u32::from_be_bytes(*b"semc"),
    Mutex = u32::from_be_bytes(*b"mute"),
}

impl PartialOrd for SyncType {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SyncType {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.wait_event().cmp(&other.wait_event())
    }
}

impl SyncType {
    /// Return the wait-queue event tag for this synchronization type.
    pub const fn wait_event(self) -> u32 {
        self as u32
    }

    /// Decode a wait-queue event tag into a synchronization type.
    pub const fn from_wait_event(wait_event: u32) -> Option<Self> {
        if wait_event == Self::BinarySemaphore.wait_event() {
            Some(Self::BinarySemaphore)
        } else if wait_event == Self::CountingSemaphore.wait_event() {
            Some(Self::CountingSemaphore)
        } else if wait_event == Self::Mutex.wait_event() {
            Some(Self::Mutex)
        } else {
            None
        }
    }
}

const WAIT_FOREVER_TICKS: u64 = u64::MAX;

type WaitTree = RBTree<SyncEntity>;

#[repr(C)]
pub(crate) struct SyncEntity {
    deadline_at: u64,
    rb_node: RbNode,
}

impl SyncEntity {
    pub(crate) const fn new() -> Self {
        Self {
            deadline_at: 0,
            rb_node: RbNode::new(),
        }
    }

    fn set_deadline_at(&mut self, deadline_at: u64) {
        self.deadline_at = deadline_at;
    }

    #[allow(dead_code)]
    fn deadline_at(&self) -> u64 {
        self.deadline_at
    }

    fn reset_links(&mut self) {
        self.rb_node.reset_links();
    }
}

unsafe impl RBTreeNode for SyncEntity {
    fn node(entity: *mut Self) -> *mut RbNode {
        if entity.is_null() {
            ptr::null_mut()
        } else {
            unsafe { ptr::addr_of_mut!((*entity).rb_node) }
        }
    }

    fn entity_of(node: *mut RbNode) -> *mut Self {
        if node.is_null() {
            ptr::null_mut()
        } else {
            unsafe {
                (node as *mut u8)
                    .sub(offset_of!(SyncEntity, rb_node))
                    .cast::<SyncEntity>()
            }
        }
    }

    fn entity_of_const(node: *const RbNode) -> *const Self {
        if node.is_null() {
            ptr::null()
        } else {
            unsafe {
                (node as *const u8)
                    .sub(offset_of!(SyncEntity, rb_node))
                    .cast::<SyncEntity>()
            }
        }
    }

    unsafe fn cmp(a: *const Self, b: *const Self) -> core::cmp::Ordering {
        unsafe {
            match (*a).deadline_at.cmp(&(*b).deadline_at) {
                core::cmp::Ordering::Equal => (a as usize).cmp(&(b as usize)),
                other => other,
            }
        }
    }
}

trait SyncWaitObject {
    const SYNC_TYPE: SyncType;

    fn waiters_mut(&mut self) -> &mut WaitTree;
}

/// Error returned by semaphore operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemaphoreError {
    /// The semaphore is not currently available for a non-blocking take.
    WouldBlock,
    /// The semaphore is already available and cannot hold another token.
    Full,
    /// A blocking take was requested before the scheduler selected a thread.
    NoCurrentThread,
    /// The scheduler could not move a thread between wait and ready queues.
    WaitQueue(WaitQueueError),
}

impl From<WaitQueueError> for SemaphoreError {
    fn from(error: WaitQueueError) -> Self {
        Self::WaitQueue(error)
    }
}

struct BinarySemaphoreState {
    available: AtomicUsize,
    waiters: WaitTree,
}

impl BinarySemaphoreState {
    const fn new(available: bool) -> Self {
        Self {
            available: AtomicUsize::new(available as usize),
            waiters: WaitTree::new(),
        }
    }
}

impl SyncWaitObject for BinarySemaphoreState {
    const SYNC_TYPE: SyncType = SyncType::BinarySemaphore;

    fn waiters_mut(&mut self) -> &mut WaitTree {
        &mut self.waiters
    }
}

/// A no-heap binary semaphore.
///
/// The semaphore stores a single token. `try_take` consumes that token when it
/// is available. `take` blocks the current scheduler thread when the token is
/// unavailable, and `give` either wakes the blocked waiter with the earliest
/// scheduler deadline or restores the token for a future taker.
pub struct BinarySemaphore {
    state: UnsafeCell<BinarySemaphoreState>,
}

unsafe impl Sync for BinarySemaphore {}

impl BinarySemaphore {
    /// Create a binary semaphore.
    ///
    /// Pass `true` to create it with an available token, or `false` to require
    /// a later `give` before the first take can complete.
    pub const fn new(available: bool) -> Self {
        Self {
            state: UnsafeCell::new(BinarySemaphoreState::new(available)),
        }
    }

    /// Create a binary semaphore with an available token.
    pub const fn available() -> Self {
        Self::new(true)
    }

    /// Create a binary semaphore with no available token.
    pub const fn empty() -> Self {
        Self::new(false)
    }

    /// Return whether the semaphore currently has a token available.
    pub fn is_available(&self) -> bool {
        unsafe { load_word(&(*self.state.get()).available) != 0 }
    }

    /// Try to take the semaphore without blocking.
    pub fn try_take(&self) -> Result<(), SemaphoreError> {
        unsafe {
            if decrement_word_if_positive(&(*self.state.get()).available) {
                Ok(())
            } else {
                Err(SemaphoreError::WouldBlock)
            }
        }
    }

    /// Take the semaphore, blocking the current scheduler thread if needed.
    pub fn take(&self) -> Result<(), SemaphoreError> {
        if self.try_take().is_ok() {
            return Ok(());
        }

        critical_section(|| unsafe {
            let state = &mut *self.state.get();
            if decrement_word_if_positive(&state.available) {
                return Ok(());
            }

            let Some(current_thread) = current_thread_handle() else {
                return Err(SemaphoreError::NoCurrentThread);
            };

            block_current_thread_on(state, current_thread)?;
            Ok(())
        })
    }

    /// Give the semaphore.
    ///
    /// If one or more threads are blocked in `take`, the earliest-deadline
    /// waiter is made ready and receives the token directly. Otherwise this
    /// restores the token for a future taker, unless the semaphore is already
    /// full.
    pub fn give(&self) -> Result<(), SemaphoreError> {
        critical_section(|| unsafe {
            let state = &mut *self.state.get();

            if let Some(waiter) = pop_waiting_thread(&mut state.waiters) {
                wake_waiter(waiter)?;
                request_context_switch();
                return Ok(());
            }

            if increment_word_if_below(&state.available, 1) {
                Ok(())
            } else {
                Err(SemaphoreError::Full)
            }
        })
    }
}

struct CountingSemaphoreState {
    count: AtomicUsize,
    max_count: AtomicUsize,
    waiters: WaitTree,
}

impl CountingSemaphoreState {
    const fn new(max_count: u32, initial_count: u32) -> Self {
        Self {
            count: AtomicUsize::new(initial_count as usize),
            max_count: AtomicUsize::new(max_count as usize),
            waiters: WaitTree::new(),
        }
    }
}

impl SyncWaitObject for CountingSemaphoreState {
    const SYNC_TYPE: SyncType = SyncType::CountingSemaphore;

    fn waiters_mut(&mut self) -> &mut WaitTree {
        &mut self.waiters
    }
}

/// A no-heap counting semaphore.
///
/// The semaphore stores up to `max_count` tokens. `try_take` consumes one token
/// when available. `take` blocks the current scheduler thread when no tokens
/// are available, and `give` either wakes the blocked waiter with the earliest
/// scheduler deadline or stores one token for a future taker.
pub struct CountingSemaphore {
    state: UnsafeCell<CountingSemaphoreState>,
}

unsafe impl Sync for CountingSemaphore {}

impl CountingSemaphore {
    /// Create a counting semaphore.
    ///
    /// `max_count` must be non-zero and `initial_count` must not exceed
    /// `max_count`.
    pub const fn new(max_count: u32, initial_count: u32) -> Self {
        assert!(
            max_count != 0,
            "counting semaphore max_count must be non-zero"
        );
        assert!(
            initial_count <= max_count,
            "counting semaphore initial_count must not exceed max_count"
        );

        Self {
            state: UnsafeCell::new(CountingSemaphoreState::new(max_count, initial_count)),
        }
    }

    /// Create a counting semaphore with no available tokens.
    pub const fn empty(max_count: u32) -> Self {
        Self::new(max_count, 0)
    }

    /// Create a counting semaphore with all tokens available.
    pub const fn full(max_count: u32) -> Self {
        Self::new(max_count, max_count)
    }

    /// Return the current number of available tokens.
    pub fn count(&self) -> u32 {
        unsafe { load_word(&(*self.state.get()).count) as u32 }
    }

    /// Return the maximum number of tokens this semaphore can store.
    pub fn max_count(&self) -> u32 {
        unsafe { load_word(&(*self.state.get()).max_count) as u32 }
    }

    /// Try to take one token without blocking.
    pub fn try_take(&self) -> Result<(), SemaphoreError> {
        unsafe {
            if decrement_word_if_positive(&(*self.state.get()).count) {
                Ok(())
            } else {
                Err(SemaphoreError::WouldBlock)
            }
        }
    }

    /// Take one token, blocking the current scheduler thread if needed.
    pub fn take(&self) -> Result<(), SemaphoreError> {
        if self.try_take().is_ok() {
            return Ok(());
        }

        critical_section(|| unsafe {
            let state = &mut *self.state.get();
            if decrement_word_if_positive(&state.count) {
                return Ok(());
            }

            let Some(current_thread) = current_thread_handle() else {
                return Err(SemaphoreError::NoCurrentThread);
            };

            block_current_thread_on(state, current_thread)?;
            Ok(())
        })
    }

    /// Give one token.
    ///
    /// If one or more threads are blocked in `take`, the earliest-deadline
    /// waiter is made ready and receives the token directly. Otherwise the
    /// token is stored unless the semaphore is already full.
    pub fn give(&self) -> Result<(), SemaphoreError> {
        critical_section(|| unsafe {
            let state = &mut *self.state.get();

            if let Some(waiter) = pop_waiting_thread(&mut state.waiters) {
                wake_waiter(waiter)?;
                request_context_switch();
                return Ok(());
            }

            if increment_word_if_below(&state.count, load_word(&state.max_count)) {
                Ok(())
            } else {
                Err(SemaphoreError::Full)
            }
        })
    }
}

/// Error returned by mutex operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutexError {
    /// The mutex is owned by another thread and cannot be taken without blocking.
    WouldBlock,
    /// The current thread already owns this non-recursive mutex.
    WouldDeadlock,
    /// A lock was requested before the scheduler selected a thread.
    NoCurrentThread,
    /// The scheduler could not move a thread between wait and ready queues.
    WaitQueue(WaitQueueError),
}

impl From<WaitQueueError> for MutexError {
    fn from(error: WaitQueueError) -> Self {
        Self::WaitQueue(error)
    }
}

struct MutexState {
    owner: AtomicUsize,
    waiters: WaitTree,
    owner_original_deadline_at: u64,
    owner_boosted: bool,
}

impl MutexState {
    const fn new() -> Self {
        Self {
            owner: AtomicUsize::new(0),
            waiters: WaitTree::new(),
            owner_original_deadline_at: 0,
            owner_boosted: false,
        }
    }
}

impl SyncWaitObject for MutexState {
    const SYNC_TYPE: SyncType = SyncType::Mutex;

    fn waiters_mut(&mut self) -> &mut WaitTree {
        &mut self.waiters
    }
}

/// A no-heap, non-recursive scheduler mutex protecting a value of type `T`.
///
/// The mutex is owned by a scheduler thread while a [`MutexGuard`] exists. If
/// another thread calls `lock`, it is moved to the wait queue and the guard
/// returned to it once the current owner drops its guard and it has the
/// earliest scheduler deadline among the mutex waiters.
pub struct Mutex<T> {
    state: UnsafeCell<MutexState>,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Create a mutex containing `value`.
    pub const fn new(value: T) -> Self {
        Self {
            state: UnsafeCell::new(MutexState::new()),
            data: UnsafeCell::new(value),
        }
    }

    /// Consume the mutex and return its inner value.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }

    /// Return a mutable reference to the inner value.
    ///
    /// This requires unique access to the mutex, so no scheduler locking is
    /// needed.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    /// Return whether the mutex is currently owned by a thread.
    pub fn is_locked(&self) -> bool {
        unsafe { load_word(&(*self.state.get()).owner) != 0 }
    }

    /// Try to lock the mutex without blocking.
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, MutexError> {
        critical_section(|| unsafe {
            let current_thread = current_thread_handle().ok_or(MutexError::NoCurrentThread)?;
            let state = &mut *self.state.get();
            let current_owner = owner_token(current_thread);

            match compare_exchange_word(&state.owner, 0, current_owner) {
                Ok(_) => Ok(MutexGuard::new(self)),
                Err(owner) if owner == current_owner => Err(MutexError::WouldDeadlock),
                Err(_) => Err(MutexError::WouldBlock),
            }
        })
    }

    /// Lock the mutex, blocking the current scheduler thread if needed.
    pub fn lock(&self) -> Result<MutexGuard<'_, T>, MutexError> {
        critical_section(|| unsafe {
            let current_thread = current_thread_handle().ok_or(MutexError::NoCurrentThread)?;
            let state = &mut *self.state.get();
            let current_owner = owner_token(current_thread);

            match compare_exchange_word(&state.owner, 0, current_owner) {
                Ok(_) => {}
                Err(owner) if owner == current_owner => {
                    return Err(MutexError::WouldDeadlock);
                }
                Err(owner) => {
                    let waiter_deadline_at = block_current_thread_on(state, current_thread)?;
                    boost_owner_deadline(state, owner, current_thread, waiter_deadline_at);
                }
            }

            Ok(MutexGuard::new(self))
        })
    }

    fn unlock_guard(&self) -> Result<(), MutexError> {
        critical_section(|| unsafe {
            let current_owner = current_thread_handle().map(owner_token).unwrap_or_default();
            let state = &mut *self.state.get();
            let observed_owner = load_word(&state.owner);

            debug_assert_eq!(
                observed_owner, current_owner,
                "mutex guard must be dropped by its owning thread"
            );

            if observed_owner != current_owner {
                return Ok(());
            }

            restore_owner_deadline(state, current_owner);

            if let Some(waiter) = pop_waiting_thread(&mut state.waiters) {
                let _ = compare_exchange_word(&state.owner, current_owner, owner_token(waiter));
                wake_waiter(waiter)?;
                request_context_switch();
            } else {
                let _ = compare_exchange_word(&state.owner, current_owner, 0);
            }

            Ok(())
        })
    }
}

/// RAII guard returned by [`Mutex::lock`] and [`Mutex::try_lock`].
///
/// Dropping the guard unlocks the mutex and wakes the earliest-deadline waiting
/// thread, if one exists.
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, T> MutexGuard<'a, T> {
    fn new(mutex: &'a Mutex<T>) -> Self {
        Self {
            mutex,
            _not_send: PhantomData,
        }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        let _ = self.mutex.unlock_guard();
    }
}

fn load_word(word: &AtomicUsize) -> usize {
    word.load(Ordering::Acquire)
}

fn compare_exchange_word(word: &AtomicUsize, current: usize, new: usize) -> Result<usize, usize> {
    word.compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
}

fn decrement_word_if_positive(word: &AtomicUsize) -> bool {
    word.fetch_update(Ordering::AcqRel, Ordering::Acquire, |observed| {
        observed.checked_sub(1)
    })
    .is_ok()
}

fn increment_word_if_below(word: &AtomicUsize, max: usize) -> bool {
    word.fetch_update(Ordering::AcqRel, Ordering::Acquire, |observed| {
        if observed < max {
            Some(observed + 1)
        } else {
            None
        }
    })
    .is_ok()
}

fn owner_token(thread: ThreadHandle) -> usize {
    let token = thread.as_ptr() as usize;
    debug_assert_ne!(token, 0, "thread owner token must be non-zero");
    token
}

unsafe fn thread_from_owner_token(token: usize) -> Option<ThreadHandle> {
    if token == 0 {
        None
    } else {
        Some(unsafe { ThreadHandle::from_thread_ctx(token as *mut _) })
    }
}

unsafe fn boost_owner_deadline(
    state: &mut MutexState,
    owner: usize,
    waiter: ThreadHandle,
    waiter_deadline_at: u64,
) {
    let Some(owner) = (unsafe { thread_from_owner_token(owner) }) else {
        return;
    };

    let boost_deadline_at = unsafe {
        if (*owner.as_ptr()).is_cfs && (*waiter.as_ptr()).is_cfs {
            return;
        }

        waiter_deadline_at.min(earliest_queued_scheduler_deadline_at())
    };

    let owner_deadline_at = unsafe { thread_scheduler_deadline_at(owner) };
    if boost_deadline_at >= owner_deadline_at {
        return;
    }

    if !state.owner_boosted {
        state.owner_original_deadline_at = owner_deadline_at;
        state.owner_boosted = true;
    }

    unsafe {
        set_thread_scheduler_deadline_at(owner, boost_deadline_at);
    }
}

unsafe fn restore_owner_deadline(state: &mut MutexState, owner: usize) {
    if !state.owner_boosted {
        return;
    }

    if let Some(owner) = unsafe { thread_from_owner_token(owner) } {
        unsafe {
            set_thread_scheduler_deadline_at(owner, state.owner_original_deadline_at);
        }
    }

    state.owner_original_deadline_at = 0;
    state.owner_boosted = false;
}

unsafe fn current_thread_handle() -> Option<ThreadHandle> {
    unsafe {
        if CURRENT_THREAD_CTX.is_null() {
            None
        } else {
            Some(ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX))
        }
    }
}

unsafe fn block_current_thread(
    thread: ThreadHandle,
    sync_type: SyncType,
) -> Result<u64, WaitQueueError> {
    unsafe {
        let current_thread_ctx = CURRENT_THREAD_CTX;

        debug_assert_eq!(
            thread.as_ptr(),
            current_thread_ctx,
            "only the current thread can block on a sync primitive"
        );

        let elapsed = elapsed_ticks_since_current_reload();
        let current_ktimer = if CURRENT_THREAD_IS_CFS {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(thread)
        };

        let deadline_at = if current_ktimer.is_null() {
            WAIT_FOREVER_TICKS
        } else {
            (*current_ktimer).deadline_at()
        };

        let _ = yield_ktimer(current_ktimer, elapsed, false);

        let wait_entity = wait_entity(thread);
        (*wait_entity).wake_at = WAIT_FOREVER_TICKS;
        (*wait_entity).waitevt = Some(sync_type);

        if CURRENT_THREAD_IS_CFS {
            dequeue_runq_to_waitq(thread)?;
        } else {
            dequeue_ktimerq_to_waitq(thread)?;
        }

        Ok(deadline_at)
    }
}

unsafe fn block_current_thread_on<T: SyncWaitObject>(
    object: &mut T,
    thread: ThreadHandle,
) -> Result<u64, WaitQueueError> {
    unsafe {
        let deadline_at = block_current_thread(thread, T::SYNC_TYPE)?;
        let entity = sync_entity(thread);
        (*entity).set_deadline_at(deadline_at);
        (*entity).reset_links();
        object.waiters_mut().insert(entity);
        request_context_switch();

        Ok(deadline_at)
    }
}

unsafe fn pop_waiting_thread(waiters: &mut WaitTree) -> Option<ThreadHandle> {
    unsafe {
        while let Some(sync_entity) = waiters.pop_first() {
            let thread = thread_handle_from_sync_entity(sync_entity as *mut SyncEntity);
            if (*thread.as_ptr()).state == ThreadState::Waiting {
                return Some(thread);
            }
        }

        None
    }
}

unsafe fn wake_waiter(thread: ThreadHandle) -> Result<(), WaitQueueError> {
    unsafe {
        if (*thread.as_ptr()).is_cfs {
            remove_wait_thread(thread);
            crate::trace::record_wakeup(thread.as_ptr());
            enqueue_runq_from_waitq(thread);
            program_wait_ktimer();
        } else {
            enqueue_ktimerq_from_waitq(thread)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::ktimer::{RtKTimer, enqueue_ktimer, init_ktimer_queue, next_ktimer};
    use crate::rbtree::RBTree;
    use crate::runq::{CFS_RUN_QUEUE, SchedEntity, enqueue_thread};
    use crate::sched::{CURRENT_THREAD_IS_CFS, init_cfs};
    use crate::thread::{CfsThread, RtThread, ThreadCtx, cfs_sched_entity, rt_ktimer_entity};
    use crate::waitq::{WAIT_QUEUE, WaitEntity};

    fn cfs_thread(name: &'static str, priority: u32) -> CfsThread {
        CfsThread {
            thread: ThreadCtx {
                sp: 0,
                exc_return: 0,
                id: 1,
                name,
                state: ThreadState::Ready,
                is_cfs: true,
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
                is_cfs: false,
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

    #[test]
    fn sync_type_wait_events_decode_named_tags() {
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

    #[test]
    fn sync_waiters_pop_by_earliest_deadline_not_insertion_order() {
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

    #[test]
    fn blocking_sync_take_copies_current_scheduler_deadline() {
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

    #[test]
    fn mutex_lock_boosts_rt_owner_deadline_and_unlock_restores_it() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mutex = Mutex::new(7);
        let mut owner = rt_thread("owner");
        let mut owner_timer = RtKTimer::new(100, ptr::null_mut(), "owner");
        let mut waiter = rt_thread("waiter");
        let mut waiter_timer = RtKTimer::new(10, ptr::null_mut(), "waiter");

        unsafe {
            reset_scheduler_state();
            owner_timer.init_rt_thread(&mut owner);
            waiter_timer.init_rt_thread(&mut waiter);
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
            assert_eq!(owner_timer.entity.deadline_at(), 10);
            assert!(ptr::eq(next_ktimer(), owner_timer.entity_mut()));

            core::mem::forget(waiter_guard);
            owner.thread.set_state(ThreadState::Running);
            CURRENT_THREAD_CTX = &mut owner.thread;
            CURRENT_THREAD_IS_CFS = false;
        }

        drop(owner_guard);

        unsafe {
            assert_eq!(owner_timer.entity.deadline_at(), 100);
            assert_eq!(waiter.thread.state, ThreadState::Ready);
            assert!(ptr::eq(
                rt_ktimer_entity(waiter_handle),
                waiter_timer.entity_mut()
            ));

            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    #[test]
    fn mutex_lock_boosts_rt_owner_to_earliest_scheduler_deadline_and_unlock_restores_it() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mutex = Mutex::new(7);
        let mut owner = rt_thread("owner");
        let mut owner_timer = RtKTimer::new(100, ptr::null_mut(), "owner");
        let mut waiter = rt_thread("waiter");
        let mut waiter_timer = RtKTimer::new(40, ptr::null_mut(), "waiter");

        unsafe {
            reset_scheduler_state();
            owner_timer.init_rt_thread(&mut owner);
            waiter_timer.init_rt_thread(&mut waiter);
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
            assert_eq!(owner_timer.entity.deadline_at(), 25);

            core::mem::forget(waiter_guard);
            owner.thread.set_state(ThreadState::Running);
            CURRENT_THREAD_CTX = &mut owner.thread;
            CURRENT_THREAD_IS_CFS = false;
        }

        drop(owner_guard);

        unsafe {
            assert_eq!(owner_timer.entity.deadline_at(), 100);
            assert_eq!(waiter.thread.state, ThreadState::Ready);

            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    #[test]
    fn mutex_lock_boosts_cfs_owner_to_earliest_scheduler_deadline_and_unlock_restores_it() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mutex = Mutex::new(7);
        let mut owner = cfs_thread("owner", 3);
        let mut waiter = rt_thread("waiter");
        let mut waiter_timer = RtKTimer::new(10, ptr::null_mut(), "waiter");
        let mut earlier_rt = rt_thread("earlier_rt");
        let mut earlier_rt_timer = RtKTimer::new(8, ptr::null_mut(), "earlier_rt");

        unsafe {
            reset_scheduler_state();
            waiter_timer.init_rt_thread(&mut waiter);
            earlier_rt_timer.init_rt_thread(&mut earlier_rt);
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
            assert_eq!((*cfs_entity).deadline_at(), 8);

            core::mem::forget(waiter_guard);
            owner.thread.set_state(ThreadState::Running);
            CURRENT_THREAD_CTX = &mut owner.thread;
            CURRENT_THREAD_IS_CFS = true;
        }

        drop(owner_guard);

        unsafe {
            let cfs = ptr::addr_of!(CFS_KTIMER);
            let cfs_entity = ptr::addr_of!((*cfs).entity);
            assert_eq!((*cfs_entity).deadline_at(), 25);
            assert_eq!(waiter.thread.state, ThreadState::Ready);

            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    #[test]
    fn binary_semaphore_try_take_and_give_track_single_token() {
        let semaphore = BinarySemaphore::available();

        assert_eq!(semaphore.try_take(), Ok(()));
        assert!(!semaphore.is_available());
        assert_eq!(semaphore.try_take(), Err(SemaphoreError::WouldBlock));

        assert_eq!(semaphore.give(), Ok(()));
        assert!(semaphore.is_available());
        assert_eq!(semaphore.give(), Err(SemaphoreError::Full));
    }

    #[test]
    fn binary_semaphore_take_blocks_current_cfs_thread_until_give() {
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

    #[test]
    fn binary_semaphore_take_without_current_thread_reports_error() {
        let _guard = TEST_LOCK.lock().unwrap();
        let semaphore = BinarySemaphore::empty();

        unsafe {
            reset_scheduler_state();
        }

        assert_eq!(semaphore.take(), Err(SemaphoreError::NoCurrentThread));
    }

    #[test]
    fn counting_semaphore_try_take_and_give_track_bounded_tokens() {
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

    #[test]
    fn counting_semaphore_take_blocks_current_cfs_thread_until_give() {
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

    #[test]
    fn counting_semaphore_take_without_current_thread_reports_error() {
        let _guard = TEST_LOCK.lock().unwrap();
        let semaphore = CountingSemaphore::empty(2);

        unsafe {
            reset_scheduler_state();
        }

        assert_eq!(semaphore.take(), Err(SemaphoreError::NoCurrentThread));
    }

    #[test]
    #[should_panic(expected = "counting semaphore max_count must be non-zero")]
    fn counting_semaphore_rejects_zero_max_count() {
        let _ = CountingSemaphore::empty(0);
    }

    #[test]
    #[should_panic(expected = "counting semaphore initial_count must not exceed max_count")]
    fn counting_semaphore_rejects_initial_count_above_max_count() {
        let _ = CountingSemaphore::new(2, 3);
    }

    #[test]
    fn mutex_try_lock_protects_data_and_unlocks_on_drop() {
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

    #[test]
    fn mutex_try_lock_reports_would_block_for_other_owner() {
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

    #[test]
    fn mutex_lock_blocks_waiter_and_drop_transfers_ownership() {
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

    #[test]
    fn mutex_lock_without_current_thread_reports_error() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mutex = Mutex::new(7);

        unsafe {
            reset_scheduler_state();
        }

        assert!(matches!(mutex.try_lock(), Err(MutexError::NoCurrentThread)));
        assert!(matches!(mutex.lock(), Err(MutexError::NoCurrentThread)));
    }

    #[test]
    fn mutex_unique_access_helpers_skip_scheduler_locking() {
        let mut mutex = Mutex::new(1);

        *mutex.get_mut() = 2;

        assert_eq!(mutex.into_inner(), 2);
    }
}
