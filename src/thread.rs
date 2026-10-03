// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Core thread definitions for the runtime scheduler.

use core::ffi::c_void;
use core::fmt;
use core::mem::MaybeUninit;
use core::mem::offset_of;
use core::ptr;
use core::ptr::NonNull;

use crate::arch::platform::{
    self, THREAD_INITIAL_FRAME_WORDS, THREAD_STACK_ALIGNMENT, request_context_switch,
};
use crate::clock::ticks_per_ms;
use crate::critical_section;
use crate::ktimer::{
    CFS_KTIMER, KTimerEntity, RtKTimer, dequeue_ktimerq_to_waitq,
    elapsed_ticks_since_current_reload, enqueue_ktimer, update_next_ktimer, with_ktimer_queue,
    yield_ktimer,
};
use crate::runq::{SchedEntity, dequeue_runq_to_waitq, enqueue_thread};
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS};
use crate::sync::{SyncEntity, SyncType};
use crate::waitq::WaitEntity;

/// Global counter for assigning unique thread IDs. Accessed only
/// from the main thread during thread creation, so no synchronization
/// is needed. When dynamic thread creation is added, this should be
/// protected by a mutex or replaced with an atomic counter.
static mut NEXT_THREAD_ID: u32 = 0;

/// Scheduler-assigned thread identifier.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(u32);

impl ThreadId {
    /// Return the numeric identifier assigned when the thread was spawned.
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Opaque reference to a scheduler thread.
///
/// Handles are returned by thread builders after the caller has provided
/// storage that satisfies the builder's lifetime requirements. The handle is
/// copyable and non-owning: it identifies scheduler-owned/static thread
/// storage, but it does not manage that storage.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ThreadHandle {
    thread: NonNull<ThreadCtx>,
}

impl ThreadHandle {
    pub(crate) unsafe fn from_thread_ctx(thread: *mut ThreadCtx) -> Self {
        debug_assert!(!thread.is_null(), "thread pointer must be non-null");

        unsafe {
            Self {
                thread: NonNull::new_unchecked(thread),
            }
        }
    }

    pub(crate) fn as_ptr(self) -> *mut ThreadCtx {
        self.thread.as_ptr()
    }

    fn with_thread_ctx<R>(self, f: impl FnOnce(&ThreadCtx) -> R) -> R {
        critical_section(|| unsafe { f(self.thread.as_ref()) })
    }

    /// Return this thread's scheduler-assigned identifier.
    pub fn id(self) -> ThreadId {
        self.with_thread_ctx(ThreadCtx::id)
    }

    /// Return this thread's static diagnostic name.
    pub fn name(self) -> &'static str {
        self.with_thread_ctx(ThreadCtx::name)
    }

    /// Return this thread's current scheduler state.
    pub fn state(self) -> ThreadState {
        self.with_thread_ctx(ThreadCtx::state)
    }

    /// Return whether this handle refers to a CFS thread.
    pub fn is_cfs(self) -> bool {
        self.with_thread_ctx(ThreadCtx::is_cfs)
    }

    /// Return whether this handle refers to an RT thread.
    pub fn is_rt(self) -> bool {
        self.with_thread_ctx(ThreadCtx::is_rt)
    }

    /// Return whether this handle refers to the scheduler idle thread.
    pub fn is_idle(self) -> bool {
        self.with_thread_ctx(ThreadCtx::is_idle)
    }
}

impl fmt::Debug for ThreadHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (id, name, state, kind) = self
            .with_thread_ctx(|thread| (thread.id(), thread.name(), thread.state(), thread.kind()));

        f.debug_struct("ThreadHandle")
            .field("id", &id)
            .field("name", &name)
            .field("state", &state)
            .field("kind", &kind)
            .finish()
    }
}

/// Validation failures that can be detected before a thread is spawned.
///
/// Thread builders treat these cases as programmer errors and panic with the
/// corresponding message before writing thread storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadSpawnError {
    NullThreadStorage,
    NullStack,
    StackTooSmall {
        required_words: usize,
        actual_words: usize,
    },
    UnalignedStackTop,
    ZeroPriority,
    NullRtTimer,
}

impl ThreadSpawnError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::NullThreadStorage => "thread storage pointer must be non-null",
            Self::NullStack => "thread stack pointer must be non-null",
            Self::StackTooSmall { .. } => {
                "thread stack must reserve at least 16 words for the initial platform frame"
            }
            Self::UnalignedStackTop => "thread stack top must be 8-byte aligned",
            Self::ZeroPriority => "CFS thread priority must be non-zero",
            Self::NullRtTimer => "RT thread ktimer must be non-null",
        }
    }
}

/// Execution state for a scheduled thread.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    /// The thread is eligible to run when selected by the scheduler.
    ///
    /// Normal transitions:
    /// - `Ready -> Running` when selected by the scheduler.
    /// - `Ready -> Waiting` when moved to a wait queue before it runs.
    Ready,
    /// The thread is currently executing on the CPU.
    ///
    /// Normal transitions:
    /// - `Running -> Ready` when preempted or voluntarily yielding.
    /// - `Running -> Waiting` when sleeping or waiting for an event.
    Running,
    /// The thread cannot run until an external event or resource becomes ready.
    ///
    /// Normal transition:
    /// - `Waiting -> Ready` when its wait condition expires or is satisfied.
    ///
    /// `Waiting -> Running` is deliberately not a scheduler transition; a
    /// thread must become `Ready` before it can be selected to run.
    Waiting,
}

impl ThreadState {
    pub const fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            (Self::Ready, Self::Ready | Self::Running | Self::Waiting) => true,
            (Self::Running, Self::Ready | Self::Running | Self::Waiting) => true,
            (Self::Waiting, Self::Ready | Self::Waiting) => true,
            (Self::Waiting, Self::Running) => false,
        }
    }
}

/// Scheduler-visible concrete thread kind.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadKind {
    /// Scheduler fallback used when no normal work is runnable.
    Idle,
    /// CFS-scheduled background thread.
    Cfs,
    /// Soft real-time thread with an RT kernel timer.
    Rt,
}

impl ThreadKind {
    pub const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    pub const fn is_cfs(self) -> bool {
        matches!(self, Self::Cfs)
    }

    pub const fn is_rt(self) -> bool {
        matches!(self, Self::Rt)
    }
}

/// 8-byte aligned stack storage for platform thread contexts.
#[repr(align(8))]
pub struct AlignedStack<const N: usize>(pub [u32; N]);

impl<const N: usize> AlignedStack<N> {
    /// Return a raw pointer to the top of this stack.
    ///
    /// The platform stack initializer consumes this pointer when building a new
    /// thread frame.
    pub fn top(&mut self) -> *mut u32 {
        let top = self.0.as_mut_ptr().wrapping_add(N);
        debug_assert_eq!(
            top as usize % THREAD_STACK_ALIGNMENT,
            0,
            "thread stack top must be 8-byte aligned"
        );
        top
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedInfo {
    /// CFS priority. Must be non-zero; lower numeric values are favored.
    pub priority: u32,
    /// Raw execution ticks accumulated by the CFS scheduler.
    pub sched_tick_cnt: u64,
    /// Virtual runtime used for CFS run-queue ordering.
    pub vruntime: u64,
}

/// Common scheduler-visible thread context.
///
/// `sp` points at the saved stack frame used when restoring the thread.
/// `exc_return` is a platform restore token prepared and consumed by the
/// active context-switch backend.
#[repr(C)]
pub struct ThreadCtx {
    /// Stack pointer captured for the next restore of this thread.
    /// Stack pointer should be always placed in the first field.
    pub sp: u32,
    /// Saved platform restore token.
    /// exc_return should be always placed in the second field.
    pub exc_return: u32,
    /// Scheduler-assigned thread identifier.
    pub id: u32,
    /// Human-readable thread name for logs and diagnostics.
    pub name: &'static str,
    /// Current lifecycle state used by the scheduler.
    pub state: ThreadState,
    /// Concrete scheduler-visible thread kind.
    pub kind: ThreadKind,
}

impl ThreadCtx {
    pub const fn id(&self) -> ThreadId {
        ThreadId(self.id)
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn state(&self) -> ThreadState {
        self.state
    }

    pub const fn kind(&self) -> ThreadKind {
        self.kind
    }

    pub const fn is_idle(&self) -> bool {
        self.kind.is_idle()
    }

    pub const fn is_cfs(&self) -> bool {
        self.kind.is_cfs()
    }

    pub const fn is_rt(&self) -> bool {
        self.kind.is_rt()
    }

    pub(crate) fn set_state(&mut self, next: ThreadState) {
        debug_assert!(
            self.state.can_transition_to(next),
            "invalid thread state transition"
        );
        self.state = next;
    }
}

/// Thread control block for the scheduler idle fallback.
///
/// Idle threads carry only the common context required by the context-switch
/// backend. They deliberately have no CFS run-queue entity, RT timer entity,
/// wait entity, or sync entity.
#[repr(C)]
pub struct IdleThread {
    /// Common context. This must remain the first field because assembly and
    /// timer code use `*mut ThreadCtx` as the shared thin pointer type.
    pub(crate) thread: ThreadCtx,
}

impl IdleThread {
    pub fn thread_ctx(&self) -> &ThreadCtx {
        &self.thread
    }

    pub fn thread_ctx_mut(&mut self) -> &mut ThreadCtx {
        &mut self.thread
    }
}

/// Thread control block for CFS-scheduled threads.
#[repr(C)]
pub struct CfsThread {
    /// Common context. This must remain the first field because assembly and
    /// timer code use `*mut ThreadCtx` as the shared thin pointer type.
    pub(crate) thread: ThreadCtx,
    /// Wait entity used for wait-queue ordering.
    pub(crate) wait_entity: WaitEntity,
    /// Sync entity used for semaphore and mutex waiter ordering.
    pub(crate) sync_entity: SyncEntity,
    /// Scheduler entity used for CFS run-queue ordering.
    pub(crate) sched_entity: SchedEntity,
}

impl CfsThread {
    pub fn thread_ctx(&self) -> &ThreadCtx {
        &self.thread
    }

    pub fn thread_ctx_mut(&mut self) -> &mut ThreadCtx {
        &mut self.thread
    }

    /// Return this thread's CFS scheduling entity.
    pub(crate) fn sched_entity(&self) -> &SchedEntity {
        &self.sched_entity
    }

    /// Return a copy of this CFS thread's scheduling metrics.
    pub fn sched_info(&self) -> SchedInfo {
        let entity = self.sched_entity();
        SchedInfo {
            priority: entity.priority,
            sched_tick_cnt: entity.sched_tick_cnt(),
            vruntime: entity.vruntime(),
        }
    }

    /// Return this thread's remaining wait ticks and synchronization wait type.
    ///
    /// Timer sleeps return `None` for the wait type.
    pub fn wait_info(&self) -> (u32, Option<SyncType>) {
        wait_info_from_entity(&self.wait_entity)
    }
}

/// Thread control block for RT-scheduled threads.
#[repr(C)]
pub struct RtThread {
    /// Common context. This must remain the first field because assembly and
    /// timer code use `*mut ThreadCtx` as the shared thin pointer type.
    pub(crate) thread: ThreadCtx,
    /// Wait entity used for wait-queue ordering.
    pub(crate) wait_entity: WaitEntity,
    /// Sync entity used for semaphore and mutex waiter ordering.
    pub(crate) sync_entity: SyncEntity,
    /// KTimer entity used for KTimerQueue ordering.
    pub(crate) ktimer_entity: *mut KTimerEntity,
    /// Elapsed tick counter for the RT thread's current period/job window.
    ///
    /// This is not only CPU execution time: it also includes time spent waiting
    /// after the thread yields into the wait queue, so RT deadlines continue to
    /// advance while the thread is sleeping. The counter is charged when the RT
    /// thread yields or is preempted, and also when it wakes from the wait queue.
    /// It is reset when the RT thread starts a new period.
    pub(crate) runtime: u32,
}

impl RtThread {
    pub fn thread_ctx(&self) -> &ThreadCtx {
        &self.thread
    }

    pub fn thread_ctx_mut(&mut self) -> &mut ThreadCtx {
        &mut self.thread
    }

    pub(crate) fn ktimer_entity(&self) -> Option<NonNull<KTimerEntity>> {
        NonNull::new(self.ktimer_entity)
    }

    pub fn has_ktimer(&self) -> bool {
        self.ktimer_entity().is_some()
    }

    pub fn runtime(&self) -> u32 {
        self.runtime
    }

    /// Return this thread's remaining wait ticks and synchronization wait type.
    ///
    /// Timer sleeps return `None` for the wait type.
    pub fn wait_info(&self) -> (u32, Option<SyncType>) {
        wait_info_from_entity(&self.wait_entity)
    }
}

#[derive(Clone, Copy)]
pub enum ThreadRef<'a> {
    Cfs(&'a CfsThread),
    Rt(&'a RtThread),
}

impl ThreadRef<'_> {
    pub fn thread_ctx(&self) -> &ThreadCtx {
        match self {
            Self::Cfs(thread) => thread.thread_ctx(),
            Self::Rt(thread) => thread.thread_ctx(),
        }
    }
}

fn wait_info_from_entity(entity: &WaitEntity) -> (u32, Option<SyncType>) {
    let now_ticks = with_ktimer_queue(|queue| queue.now_ticks());
    (entity.remaining_at(now_ticks), entity.waitevt)
}

/// Scheduler-class-specific initialization for concrete thread control blocks.
pub trait ThreadControlBlock {
    const KIND: ThreadKind;

    /// Scheduler-class-specific initialization argument.
    type InitArgs;

    /// Initialize the concrete thread storage and return its common thread pointer.
    ///
    /// # Safety
    ///
    /// `thread` must be non-null, properly aligned, uniquely owned writable
    /// storage for `Self`, and it must not already hold a thread that is
    /// visible to the scheduler.
    ///
    /// The storage backing `thread`, the stack described by `common`, and any
    /// scheduler-class resources in `args` must outlive all scheduler use of
    /// the returned `ThreadCtx`. The scheduler queues required by the concrete
    /// thread class must already be initialized, and the caller must serialize
    /// this initialization against scheduler interrupts and other thread
    /// creation.
    unsafe fn init(thread: *mut Self, common: ThreadCtx, args: Self::InitArgs) -> *mut ThreadCtx;

    fn validate_init_args(_args: &Self::InitArgs) -> Result<(), ThreadSpawnError> {
        Ok(())
    }
}

impl ThreadControlBlock for CfsThread {
    const KIND: ThreadKind = ThreadKind::Cfs;
    type InitArgs = u32;

    fn validate_init_args(priority: &Self::InitArgs) -> Result<(), ThreadSpawnError> {
        if *priority == 0 {
            Err(ThreadSpawnError::ZeroPriority)
        } else {
            Ok(())
        }
    }

    unsafe fn init(
        thread: *mut Self,
        common: ThreadCtx,
        priority: Self::InitArgs,
    ) -> *mut ThreadCtx {
        assert!(priority != 0, "CFS thread priority must be non-zero");

        unsafe {
            ptr::write(
                thread,
                CfsThread {
                    thread: common,
                    wait_entity: WaitEntity::new(),
                    sync_entity: SyncEntity::new(),
                    sched_entity: SchedEntity::new(priority),
                },
            );
            let common_thread = ptr::addr_of_mut!((*thread).thread);
            enqueue_thread(ThreadHandle::from_thread_ctx(common_thread));
            common_thread
        }
    }
}

impl ThreadControlBlock for RtThread {
    const KIND: ThreadKind = ThreadKind::Rt;
    type InitArgs = *mut RtKTimer;

    fn validate_init_args(ktimer: &Self::InitArgs) -> Result<(), ThreadSpawnError> {
        if ktimer.is_null() {
            Err(ThreadSpawnError::NullRtTimer)
        } else {
            Ok(())
        }
    }

    unsafe fn init(thread: *mut Self, common: ThreadCtx, ktimer: Self::InitArgs) -> *mut ThreadCtx {
        assert!(!ktimer.is_null(), "RT thread ktimer must be non-null");

        unsafe {
            ptr::write(
                thread,
                RtThread {
                    thread: common,
                    wait_entity: WaitEntity::new(),
                    sync_entity: SyncEntity::new(),
                    ktimer_entity: ptr::null_mut(),
                    runtime: 0,
                },
            );
            let common_thread = ptr::addr_of_mut!((*thread).thread);
            (*ktimer).init_rt_ktimer(common_thread);
            enqueue_ktimer((*ktimer).entity_mut());
            common_thread
        }
    }
}

pub type ThreadEntry = extern "C" fn(*mut c_void) -> !;

#[derive(Clone, Copy)]
pub struct ThreadStart {
    name: &'static str,
    entry: ThreadEntry,
    arg: *mut c_void,
}

impl ThreadStart {
    pub const fn new(name: &'static str, entry: ThreadEntry) -> Self {
        Self {
            name,
            entry,
            arg: ptr::null_mut(),
        }
    }

    pub const fn with_arg(mut self, arg: *mut c_void) -> Self {
        self.arg = arg;
        self
    }
}

pub struct CfsThreadBuilder {
    start: ThreadStart,
    priority: u32,
}

impl CfsThreadBuilder {
    /// Create a CFS thread builder.
    ///
    /// `priority` must be non-zero. Lower numeric priority values are favored
    /// because they accumulate CFS `vruntime` more slowly.
    pub const fn new(name: &'static str, entry: ThreadEntry, priority: u32) -> Self {
        Self {
            start: ThreadStart::new(name, entry),
            priority,
        }
    }

    pub const fn with_arg(mut self, arg: *mut c_void) -> Self {
        self.start = self.start.with_arg(arg);
        self
    }

    /// Initialize a CFS thread from typed storage and stack objects.
    ///
    /// # Safety
    ///
    /// `thread` must be non-null, properly aligned, uniquely owned writable
    /// storage for one `CfsThread`. `stack` must be non-null, uniquely owned
    /// writable storage for the thread stack, with a top address aligned for
    /// the active platform and at least `THREAD_INITIAL_FRAME_WORDS` words
    /// available for the initial frame.
    ///
    /// Both storage objects must remain at fixed addresses and outlive all
    /// scheduler use of the returned handle. Call this after `init_cfs` and
    /// while thread creation and scheduler globals are not being concurrently
    /// accessed. The entry function must use the `ThreadEntry` ABI and must not
    /// return; any argument pointer supplied with `with_arg` must remain valid
    /// for the entry function's use.
    pub unsafe fn spawn<const N: usize>(
        self,
        thread: *mut MaybeUninit<CfsThread>,
        stack: *mut AlignedStack<N>,
    ) -> ThreadHandle {
        unsafe { spawn_thread(thread, stack, self.start, self.priority) }
    }
}

pub(crate) unsafe fn spawn_idle_thread<const N: usize>(
    start: ThreadStart,
    thread: *mut MaybeUninit<IdleThread>,
    stack: *mut AlignedStack<N>,
) -> ThreadHandle {
    validate_thread_storage(thread).unwrap_or_else(|error| panic!("{}", error.message()));
    validate_stack(stack).unwrap_or_else(|error| panic!("{}", error.message()));

    unsafe {
        let initial_context = platform::init_thread_stack(stack_top(stack), start.entry, start.arg);
        let id = NEXT_THREAD_ID;
        NEXT_THREAD_ID = NEXT_THREAD_ID.wrapping_add(1);

        ptr::write(
            thread.cast::<IdleThread>(),
            IdleThread {
                thread: ThreadCtx {
                    sp: initial_context.sp,
                    exc_return: initial_context.exc_return,
                    id,
                    name: start.name,
                    state: ThreadState::Ready,
                    kind: ThreadKind::Idle,
                },
            },
        );

        let common_thread = ptr::addr_of_mut!((*thread.cast::<IdleThread>()).thread);
        ThreadHandle::from_thread_ctx(common_thread)
    }
}

pub struct RtThreadBuilder {
    start: ThreadStart,
    ktimer: *mut RtKTimer,
}

impl RtThreadBuilder {
    pub const fn new(name: &'static str, entry: ThreadEntry, ktimer: *mut RtKTimer) -> Self {
        Self {
            start: ThreadStart::new(name, entry),
            ktimer,
        }
    }

    pub const fn with_arg(mut self, arg: *mut c_void) -> Self {
        self.start = self.start.with_arg(arg);
        self
    }

    /// Initialize an RT thread from typed storage, stack, and timer objects.
    ///
    /// # Safety
    ///
    /// `thread` must be non-null, properly aligned, uniquely owned writable
    /// storage for one `RtThread`. `stack` must be non-null, uniquely owned
    /// writable storage for the thread stack, with a top address aligned for
    /// the active platform and at least `THREAD_INITIAL_FRAME_WORDS` words
    /// available for the initial frame.
    ///
    /// The `RtKTimer` pointer supplied to `new` must be non-null, uniquely
    /// owned by this thread, and not already queued for another thread. The
    /// thread, stack, and timer storage must remain at fixed addresses and
    /// outlive all scheduler use of the returned handle. Call this after
    /// `init_ktimer_queue` and while thread creation and scheduler globals are
    /// not being concurrently accessed. The entry function must use the
    /// `ThreadEntry` ABI and must not return; any argument pointer supplied
    /// with `with_arg` must remain valid for the entry function's use.
    pub unsafe fn spawn<const N: usize>(
        self,
        thread: *mut MaybeUninit<RtThread>,
        stack: *mut AlignedStack<N>,
    ) -> ThreadHandle {
        unsafe { spawn_thread(thread, stack, self.start, self.ktimer) }
    }
}

unsafe fn spawn_thread<T: ThreadControlBlock, const N: usize>(
    thread: *mut MaybeUninit<T>,
    stack: *mut AlignedStack<N>,
    start: ThreadStart,
    init_args: T::InitArgs,
) -> ThreadHandle {
    T::validate_init_args(&init_args).unwrap_or_else(|error| panic!("{}", error.message()));
    validate_thread_storage(thread).unwrap_or_else(|error| panic!("{}", error.message()));
    validate_stack(stack).unwrap_or_else(|error| panic!("{}", error.message()));

    unsafe {
        forkyi(
            thread.cast::<T>(),
            stack_top(stack),
            start.entry,
            start.arg,
            start.name,
            init_args,
        )
    }
}

fn validate_thread_storage<T>(thread: *mut MaybeUninit<T>) -> Result<(), ThreadSpawnError> {
    if thread.is_null() {
        Err(ThreadSpawnError::NullThreadStorage)
    } else {
        Ok(())
    }
}

fn validate_stack<const N: usize>(stack: *mut AlignedStack<N>) -> Result<(), ThreadSpawnError> {
    if stack.is_null() {
        return Err(ThreadSpawnError::NullStack);
    }

    if N < THREAD_INITIAL_FRAME_WORDS {
        return Err(ThreadSpawnError::StackTooSmall {
            required_words: THREAD_INITIAL_FRAME_WORDS,
            actual_words: N,
        });
    }

    let top = unsafe { (*stack).0.as_ptr().wrapping_add(N) };
    if (top as usize) & (THREAD_STACK_ALIGNMENT - 1) != 0 {
        return Err(ThreadSpawnError::UnalignedStackTop);
    }

    Ok(())
}

unsafe fn stack_top<const N: usize>(stack: *mut AlignedStack<N>) -> *mut u32 {
    debug_assert!(!stack.is_null(), "thread stack pointer must be non-null");

    unsafe { (*stack).top() }
}

/// Low-level thread initializer used by the typed thread builders.
///
/// Prefer `CfsThreadBuilder::spawn` or `RtThreadBuilder::spawn` unless a board
/// port needs to provide scheduler-class-specific storage itself.
///
/// # Safety
///
/// `thread` must be non-null, properly aligned, uniquely owned writable
/// storage for `T`, and it must not already be initialized or linked into any
/// scheduler queue. `sp` must be the exclusive top-of-stack pointer for the
/// thread, aligned for the active platform, with at least
/// `THREAD_INITIAL_FRAME_WORDS` writable words below it.
///
/// The thread storage, stack storage, and any resources carried in `init_args`
/// must remain at fixed addresses and outlive all scheduler use of the returned
/// handle. `init_args` must satisfy `T::init` for the concrete scheduler
/// class, including a non-zero CFS priority or a live, unqueued RT timer as
/// appropriate.
///
/// Call this only after the scheduler queues required by `T` have been
/// initialized and while thread creation and scheduler globals are not being
/// concurrently accessed. `entry` must use the `ThreadEntry` ABI and must not
/// return. `arg` is passed through unchanged; the caller must keep it valid for
/// whatever `entry` does with it.
pub unsafe fn forkyi<T: ThreadControlBlock>(
    thread: *mut T,
    sp: *mut u32,
    entry: ThreadEntry,
    arg: *mut c_void,
    name: &'static str,
    init_args: T::InitArgs,
) -> ThreadHandle {
    debug_assert!(!thread.is_null(), "thread storage pointer must be non-null");
    debug_assert!(!sp.is_null(), "thread stack pointer must be non-null");
    debug_assert_eq!(
        sp as usize % THREAD_STACK_ALIGNMENT,
        0,
        "thread stack top must be 8-byte aligned"
    );

    unsafe {
        let initial_context = platform::init_thread_stack(sp, entry, arg);
        let id = NEXT_THREAD_ID;
        NEXT_THREAD_ID = NEXT_THREAD_ID.wrapping_add(1);
        let common = ThreadCtx {
            sp: initial_context.sp,
            exc_return: initial_context.exc_return,
            id,
            name,
            state: ThreadState::Ready,
            kind: T::KIND,
        };
        ThreadHandle::from_thread_ctx(T::init(thread, common, init_args))
    }
}

pub(crate) unsafe fn cfs_thread_from_handle(thread: ThreadHandle) -> *mut CfsThread {
    let thread = thread.as_ptr();

    (thread as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, thread))
        .cast::<CfsThread>()
}

pub(crate) unsafe fn cfs_sched_entity(thread: ThreadHandle) -> *mut SchedEntity {
    unsafe {
        let cfs_thread = cfs_thread_from_handle(thread);
        ptr::addr_of_mut!((*cfs_thread).sched_entity)
    }
}

pub(crate) unsafe fn thread_handle_from_cfs_sched_entity(entity: *mut SchedEntity) -> ThreadHandle {
    debug_assert!(!entity.is_null());

    let cfs_thread = (entity as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, sched_entity))
        .cast::<CfsThread>();

    unsafe { ThreadHandle::from_thread_ctx(ptr::addr_of_mut!((*cfs_thread).thread)) }
}

pub(crate) unsafe fn cfs_wait_entity(thread: ThreadHandle) -> *mut WaitEntity {
    let thread = thread.as_ptr();
    let cfs_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, thread))
        .cast::<CfsThread>();

    unsafe { ptr::addr_of_mut!((*cfs_thread).wait_entity) }
}

pub(crate) unsafe fn rt_wait_entity(thread: ThreadHandle) -> *mut WaitEntity {
    let thread = thread.as_ptr();
    let rt_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>();

    unsafe { ptr::addr_of_mut!((*rt_thread).wait_entity) }
}

pub(crate) unsafe fn cfs_sync_entity(thread: ThreadHandle) -> *mut SyncEntity {
    let thread = thread.as_ptr();
    let cfs_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, thread))
        .cast::<CfsThread>();

    unsafe { ptr::addr_of_mut!((*cfs_thread).sync_entity) }
}

pub(crate) unsafe fn rt_sync_entity(thread: ThreadHandle) -> *mut SyncEntity {
    let thread = thread.as_ptr();
    let rt_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>();

    unsafe { ptr::addr_of_mut!((*rt_thread).sync_entity) }
}

pub(crate) unsafe fn sync_entity(thread: ThreadHandle) -> *mut SyncEntity {
    unsafe {
        match (*thread.as_ptr()).kind {
            ThreadKind::Cfs => cfs_sync_entity(thread),
            ThreadKind::Rt => rt_sync_entity(thread),
            ThreadKind::Idle => panic!("idle thread has no sync entity"),
        }
    }
}

pub(crate) unsafe fn rt_ktimer_entity(thread: ThreadHandle) -> *mut KTimerEntity {
    let thread = thread.as_ptr();
    let rt_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>();

    unsafe { (*rt_thread).ktimer_entity }
}

pub(crate) unsafe fn set_rt_ktimer_entity(thread: ThreadHandle, ktimer_entity: *mut KTimerEntity) {
    let thread = thread.as_ptr();
    let rt_thread = (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>();

    unsafe {
        (*rt_thread).ktimer_entity = ktimer_entity;
    }
}

pub(crate) unsafe fn thread_handle_from_wait_entity(entity: *mut WaitEntity) -> ThreadHandle {
    debug_assert!(!entity.is_null());

    debug_assert_eq!(
        offset_of!(CfsThread, wait_entity),
        offset_of!(RtThread, wait_entity)
    );

    let thread = (entity as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, wait_entity))
        .cast::<CfsThread>();

    unsafe { ThreadHandle::from_thread_ctx(ptr::addr_of_mut!((*thread).thread)) }
}

pub(crate) unsafe fn thread_handle_from_sync_entity(entity: *mut SyncEntity) -> ThreadHandle {
    debug_assert!(!entity.is_null());

    debug_assert_eq!(
        offset_of!(CfsThread, sync_entity),
        offset_of!(RtThread, sync_entity)
    );

    let thread = (entity as *mut u8)
        .wrapping_sub(offset_of!(CfsThread, sync_entity))
        .cast::<CfsThread>();

    unsafe { ThreadHandle::from_thread_ctx(ptr::addr_of_mut!((*thread).thread)) }
}

pub(crate) unsafe fn rt_thread_from_handle(thread: ThreadHandle) -> *mut RtThread {
    let thread = thread.as_ptr();
    (thread as *mut u8)
        .wrapping_sub(offset_of!(RtThread, thread))
        .cast::<RtThread>()
}

pub(crate) unsafe fn thread_ref_from_handle<'a>(thread: ThreadHandle) -> ThreadRef<'a> {
    unsafe {
        match (*thread.as_ptr()).kind {
            ThreadKind::Cfs => ThreadRef::Cfs(&*cfs_thread_from_handle(thread)),
            ThreadKind::Rt => ThreadRef::Rt(&*rt_thread_from_handle(thread)),
            ThreadKind::Idle => panic!("idle thread has no thread reference"),
        }
    }
}

/// Return a handle to the thread currently selected by the scheduler.
///
/// This reports the scheduler's current thread pointer. It returns `None`
/// before the first thread has been started or after test code explicitly
/// clears scheduler state.
pub fn current_thread() -> Option<ThreadHandle> {
    critical_section(|| unsafe {
        if CURRENT_THREAD_CTX.is_null() {
            None
        } else {
            Some(ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX))
        }
    })
}

/// Return the identifier of the thread currently selected by the scheduler.
pub fn current_thread_id() -> Option<ThreadId> {
    critical_section(|| unsafe {
        if CURRENT_THREAD_CTX.is_null() {
            None
        } else {
            Some((*CURRENT_THREAD_CTX).id())
        }
    })
}

pub fn set_rt_thread_start_time(start_time: u32) -> bool {
    unsafe {
        if !CURRENT_THREAD_CTX.is_null() && (*CURRENT_THREAD_CTX).is_rt() {
            let current_thread = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
            let rt_thread = rt_thread_from_handle(current_thread);
            (*rt_thread).runtime = start_time;
            true
        } else {
            false
        }
    }
}

pub fn current_rt_thread_runtime() -> Option<u32> {
    critical_section(|| unsafe {
        if CURRENT_THREAD_CTX.is_null() || !(*CURRENT_THREAD_CTX).is_rt() {
            None
        } else {
            let current_thread = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
            let rt_thread = rt_thread_from_handle(current_thread);
            Some((*rt_thread).runtime)
        }
    })
}

/// Cooperatively yield the CPU from the running RT thread to the
/// next scheduled left-most timer in the KTimer rbtree.
///
/// This is intended for the threads that have completed their current
/// job and want to give a chance to next scheduled thread.
pub fn yieldyi() {
    critical_section(|| unsafe {
        let elapsed: u32 = elapsed_ticks_since_current_reload();
        let current_thread = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
        let current_ktimer = match (*current_thread.as_ptr()).kind {
            ThreadKind::Cfs => ptr::addr_of_mut!(CFS_KTIMER.entity),
            ThreadKind::Rt => rt_ktimer_entity(current_thread),
            ThreadKind::Idle => return,
        };

        let next_ktimer = yield_ktimer(current_ktimer, elapsed, true);
        update_next_ktimer(next_ktimer);

        crate::diagnostics::trace::record_yield(CURRENT_THREAD_CTX, elapsed);
        request_context_switch();
    });
}

pub fn msleepyi(msec: u32) {
    critical_section(|| unsafe {
        let elapsed = elapsed_ticks_since_current_reload();
        let current_thread = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
        let current_ktimer = match (*current_thread.as_ptr()).kind {
            ThreadKind::Cfs => ptr::addr_of_mut!(CFS_KTIMER.entity),
            ThreadKind::Rt => rt_ktimer_entity(current_thread),
            ThreadKind::Idle => return,
        };

        let _ = yield_ktimer(current_ktimer, elapsed, false);

        let wait_entity = match (*current_thread.as_ptr()).kind {
            ThreadKind::Cfs => cfs_wait_entity(current_thread),
            ThreadKind::Rt => rt_wait_entity(current_thread),
            ThreadKind::Idle => return,
        };
        let now_ticks = with_ktimer_queue(|queue| queue.now_ticks());
        (*wait_entity).set_wake_after(now_ticks, msec.saturating_mul(ticks_per_ms()));
        (*wait_entity).waitevt = None;

        if CURRENT_THREAD_IS_CFS {
            let _ = dequeue_runq_to_waitq(current_thread);
        } else {
            let _ = dequeue_ktimerq_to_waitq(current_thread);
        }

        request_context_switch();
    });
}
