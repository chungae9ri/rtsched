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
    elapsed_ticks_since_current_reload, enqueue_ktimer, ktimer_now_ticks, update_next_ktimer,
    yield_ktimer,
};
use crate::runq::{SchedEntity, dequeue_runq_to_waitq, enqueue_thread};
use crate::sched::{CURRENT_THREAD_CTX, CURRENT_THREAD_IS_CFS};
use crate::sync::SyncEntity;
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
        !self.is_cfs()
    }
}

impl fmt::Debug for ThreadHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (id, name, state, is_cfs) = self.with_thread_ctx(|thread| {
            (thread.id(), thread.name(), thread.state(), thread.is_cfs())
        });

        f.debug_struct("ThreadHandle")
            .field("id", &id)
            .field("name", &name)
            .field("state", &state)
            .field("is_cfs", &is_cfs)
            .finish()
    }
}

/// Validation failures that can be reported before a thread is spawned.
///
/// Use `try_spawn` when setup code wants to handle these failures explicitly.
/// The `spawn` convenience methods treat the same cases as programmer errors
/// and panic with the corresponding message.
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
    /// Scheduler class for the concrete thread control block.
    pub is_cfs: bool,
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

    pub const fn is_cfs(&self) -> bool {
        self.is_cfs
    }

    pub(crate) fn set_state(&mut self, next: ThreadState) {
        debug_assert!(
            self.state.can_transition_to(next),
            "invalid thread state transition"
        );
        self.state = next;
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

    /// Return this thread's remaining wait ticks and wait event.
    pub fn wait_info(&self) -> (u32, u32) {
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

    /// Return this thread's remaining wait ticks and wait event.
    pub fn wait_info(&self) -> (u32, u32) {
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

fn wait_info_from_entity(entity: &WaitEntity) -> (u32, u32) {
    (entity.remaining_at(ktimer_now_ticks()), entity.waitevt)
}

/// Scheduler-class-specific initialization for concrete thread control blocks.
pub trait ThreadControlBlock {
    const IS_CFS: bool;

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
}

impl ThreadControlBlock for CfsThread {
    const IS_CFS: bool = true;
    type InitArgs = u32;

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
    const IS_CFS: bool = false;
    type InitArgs = *mut RtKTimer;

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
        unsafe {
            self.try_spawn(thread, stack)
                .unwrap_or_else(|error| panic!("{}", error.message()))
        }
    }

    /// Validate and initialize a CFS thread without panicking on setup errors.
    ///
    /// # Safety
    ///
    /// `thread` must be properly aligned, uniquely owned writable storage for
    /// one `CfsThread` when non-null. `stack` must be uniquely owned writable
    /// stack storage when non-null, with a top address aligned for the active
    /// platform and enough room for the initial frame.
    ///
    /// This method reports null pointers, undersized stacks, misaligned stack
    /// tops, and zero priority as `ThreadSpawnError`, but it cannot validate
    /// aliasing, dangling pointers, or lifetime requirements. Valid storage
    /// must remain at fixed addresses and outlive all scheduler use of the
    /// returned handle. Call this after `init_cfs` and while thread creation
    /// and scheduler globals are not being concurrently accessed. The entry
    /// function must use the `ThreadEntry` ABI and must not return; any
    /// argument pointer supplied with `with_arg` must remain valid for the
    /// entry function's use.
    pub unsafe fn try_spawn<const N: usize>(
        self,
        thread: *mut MaybeUninit<CfsThread>,
        stack: *mut AlignedStack<N>,
    ) -> Result<ThreadHandle, ThreadSpawnError> {
        if self.priority == 0 {
            return Err(ThreadSpawnError::ZeroPriority);
        }

        unsafe { try_spawn_thread(thread, stack, self.start, self.priority) }
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
        unsafe {
            self.try_spawn(thread, stack)
                .unwrap_or_else(|error| panic!("{}", error.message()))
        }
    }

    /// Validate and initialize an RT thread without panicking on setup errors.
    ///
    /// # Safety
    ///
    /// `thread` must be properly aligned, uniquely owned writable storage for
    /// one `RtThread` when non-null. `stack` must be uniquely owned writable
    /// stack storage when non-null, with a top address aligned for the active
    /// platform and enough room for the initial frame.
    ///
    /// This method reports null pointers, undersized stacks, misaligned stack
    /// tops, and a null timer pointer as `ThreadSpawnError`, but it cannot
    /// validate aliasing, dangling pointers, or lifetime requirements. The
    /// `RtKTimer` pointer supplied to `new` must be uniquely owned by this
    /// thread and not already queued for another thread. Valid thread, stack,
    /// and timer storage must remain at fixed addresses and outlive all
    /// scheduler use of the returned handle. Call this after
    /// `init_ktimer_queue` and while thread creation and scheduler globals are
    /// not being concurrently accessed. The entry function must use the
    /// `ThreadEntry` ABI and must not return; any argument pointer supplied
    /// with `with_arg` must remain valid for the entry function's use.
    pub unsafe fn try_spawn<const N: usize>(
        self,
        thread: *mut MaybeUninit<RtThread>,
        stack: *mut AlignedStack<N>,
    ) -> Result<ThreadHandle, ThreadSpawnError> {
        if self.ktimer.is_null() {
            return Err(ThreadSpawnError::NullRtTimer);
        }

        unsafe { try_spawn_thread(thread, stack, self.start, self.ktimer) }
    }
}

unsafe fn try_spawn_thread<T: ThreadControlBlock, const N: usize>(
    thread: *mut MaybeUninit<T>,
    stack: *mut AlignedStack<N>,
    start: ThreadStart,
    init_args: T::InitArgs,
) -> Result<ThreadHandle, ThreadSpawnError> {
    validate_thread_storage(thread)?;
    validate_stack(stack)?;

    unsafe {
        let handle = forkyi(
            thread.cast::<T>(),
            stack_top(stack),
            start.entry,
            start.arg,
            start.name,
            init_args,
        );

        Ok(handle)
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
            is_cfs: T::IS_CFS,
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
        if (*thread.as_ptr()).is_cfs {
            cfs_sync_entity(thread)
        } else {
            rt_sync_entity(thread)
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
        if (*thread.as_ptr()).is_cfs {
            ThreadRef::Cfs(&*cfs_thread_from_handle(thread))
        } else {
            ThreadRef::Rt(&*rt_thread_from_handle(thread))
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
        if !CURRENT_THREAD_CTX.is_null() && !CURRENT_THREAD_IS_CFS {
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
        if CURRENT_THREAD_CTX.is_null() || CURRENT_THREAD_IS_CFS {
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
        let current_ktimer = if CURRENT_THREAD_IS_CFS {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(current_thread)
        };

        let next_ktimer = yield_ktimer(current_ktimer, elapsed, true);
        update_next_ktimer(next_ktimer);

        crate::trace::record_yield(CURRENT_THREAD_CTX, elapsed);
        request_context_switch();
    });
}

pub fn msleepyi(msec: u32) {
    critical_section(|| unsafe {
        let elapsed = elapsed_ticks_since_current_reload();
        let current_thread = ThreadHandle::from_thread_ctx(CURRENT_THREAD_CTX);
        let current_ktimer = if CURRENT_THREAD_IS_CFS {
            ptr::addr_of_mut!(CFS_KTIMER.entity)
        } else {
            rt_ktimer_entity(current_thread)
        };

        let _ = yield_ktimer(current_ktimer, elapsed, false);

        let wait_entity = if CURRENT_THREAD_IS_CFS {
            cfs_wait_entity(current_thread)
        } else {
            rt_wait_entity(current_thread)
        };
        (*wait_entity).set_wake_after(ktimer_now_ticks(), msec.saturating_mul(ticks_per_ms()));
        (*wait_entity).waitevt = 0;

        if CURRENT_THREAD_IS_CFS {
            let _ = dequeue_runq_to_waitq(current_thread);
        } else {
            let _ = dequeue_ktimerq_to_waitq(current_thread);
        }

        request_context_switch();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::ktimer::{KTimerEntity, RtKTimer, init_ktimer_queue};

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
                id: 2,
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

    extern "C" fn test_entry(_arg: *mut c_void) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }

    unsafe fn thread_handle(thread: *mut ThreadCtx) -> ThreadHandle {
        unsafe { ThreadHandle::from_thread_ctx(thread) }
    }

    #[test]
    fn thread_state_transition_matrix_documents_lifecycle() {
        assert!(ThreadState::Ready.can_transition_to(ThreadState::Running));
        assert!(ThreadState::Ready.can_transition_to(ThreadState::Waiting));
        assert!(ThreadState::Running.can_transition_to(ThreadState::Ready));
        assert!(ThreadState::Running.can_transition_to(ThreadState::Waiting));
        assert!(ThreadState::Waiting.can_transition_to(ThreadState::Ready));

        assert!(!ThreadState::Waiting.can_transition_to(ThreadState::Running));
    }

    #[test]
    fn thread_context_helpers_follow_ready_running_waiting_ready_cycle() {
        let mut cfs = cfs_thread("cfs", 1);

        assert_eq!(cfs.thread.state, ThreadState::Ready);

        cfs.thread.set_state(ThreadState::Running);
        assert_eq!(cfs.thread.state, ThreadState::Running);

        cfs.thread.set_state(ThreadState::Waiting);
        assert_eq!(cfs.thread.state, ThreadState::Waiting);

        cfs.thread.set_state(ThreadState::Ready);
        assert_eq!(cfs.thread.state, ThreadState::Ready);
    }

    #[test]
    #[should_panic(expected = "invalid thread state transition")]
    fn waiting_thread_cannot_transition_directly_to_running() {
        let mut cfs = cfs_thread("cfs", 1);

        cfs.thread.set_state(ThreadState::Waiting);
        cfs.thread.set_state(ThreadState::Running);
    }

    #[test]
    fn cfs_thread_builder_initializes_typed_storage_and_stack() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut storage = MaybeUninit::<CfsThread>::uninit();
        let mut stack = AlignedStack([0; 64]);
        let arg = 0x1234usize as *mut c_void;

        unsafe {
            crate::runq::init_cfs_rq();
            let handle = CfsThreadBuilder::new("cfs", test_entry, 3)
                .with_arg(arg)
                .spawn(&mut storage, &mut stack);
            let thread = handle.as_ptr();
            let cfs = &*storage.as_ptr();

            assert!(ptr::eq(thread, ptr::addr_of!(cfs.thread).cast_mut()));
            assert_eq!(handle.id().as_u32(), (*thread).id);
            assert_eq!(handle.name(), "cfs");
            assert_eq!(handle.state(), ThreadState::Ready);
            assert!(handle.is_cfs());
            assert!(!handle.is_rt());
            assert_eq!((*thread).name, "cfs");
            assert!((*thread).is_cfs);
            assert_eq!(cfs.sched_entity.priority, 3);
            assert_eq!(
                (*thread).sp,
                stack.0.as_mut_ptr().wrapping_add(64 - 16) as usize as u32
            );
            assert_eq!(stack.0[64 - 8], arg as usize as u32);
        }
    }

    #[test]
    fn rt_thread_builder_initializes_typed_storage_stack_and_timer() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut storage = MaybeUninit::<RtThread>::uninit();
        let mut stack = AlignedStack([0; 64]);
        let mut ktimer = RtKTimer::new(10, ptr::null_mut(), "rt");

        unsafe {
            init_ktimer_queue();
            let handle =
                RtThreadBuilder::new("rt", test_entry, &mut ktimer).spawn(&mut storage, &mut stack);
            let thread = handle.as_ptr();
            let rt = &*storage.as_ptr();

            assert!(ptr::eq(thread, ptr::addr_of!(rt.thread).cast_mut()));
            assert_eq!(handle.id().as_u32(), (*thread).id);
            assert_eq!(handle.name(), "rt");
            assert_eq!(handle.state(), ThreadState::Ready);
            assert!(!handle.is_cfs());
            assert!(handle.is_rt());
            assert_eq!((*thread).name, "rt");
            assert!(!(*thread).is_cfs);
            assert_eq!(rt.runtime, 0);
            assert!(ptr::eq(
                rt.ktimer_entity().unwrap().as_ptr(),
                ktimer.entity_mut()
            ));
            assert!(ptr::eq(ktimer.thread_ctx(), thread));
        }
    }

    #[test]
    #[should_panic(
        expected = "thread stack must reserve at least 16 words for the initial platform frame"
    )]
    fn cfs_thread_builder_rejects_too_small_stack() {
        let mut storage = MaybeUninit::<CfsThread>::uninit();
        let mut stack = AlignedStack([0; THREAD_INITIAL_FRAME_WORDS - 1]);

        unsafe {
            CfsThreadBuilder::new("bad", test_entry, 1).spawn(&mut storage, &mut stack);
        }
    }

    #[test]
    fn cfs_thread_builder_try_spawn_reports_validation_errors() {
        let mut storage = MaybeUninit::<CfsThread>::uninit();
        let mut stack = AlignedStack([0; 64]);
        let mut small_stack = AlignedStack([0; THREAD_INITIAL_FRAME_WORDS - 1]);
        let mut odd_stack = AlignedStack([0; THREAD_INITIAL_FRAME_WORDS + 1]);

        unsafe {
            assert_eq!(
                CfsThreadBuilder::new("bad", test_entry, 1).try_spawn(ptr::null_mut(), &mut stack),
                Err(ThreadSpawnError::NullThreadStorage)
            );
            assert_eq!(
                CfsThreadBuilder::new("bad", test_entry, 1)
                    .try_spawn(&mut storage, ptr::null_mut::<AlignedStack<64>>()),
                Err(ThreadSpawnError::NullStack)
            );
            assert_eq!(
                CfsThreadBuilder::new("bad", test_entry, 1)
                    .try_spawn(&mut storage, &mut small_stack),
                Err(ThreadSpawnError::StackTooSmall {
                    required_words: THREAD_INITIAL_FRAME_WORDS,
                    actual_words: THREAD_INITIAL_FRAME_WORDS - 1,
                })
            );
            assert_eq!(
                CfsThreadBuilder::new("bad", test_entry, 1).try_spawn(&mut storage, &mut odd_stack),
                Err(ThreadSpawnError::UnalignedStackTop)
            );
            assert_eq!(
                CfsThreadBuilder::new("bad", test_entry, 0).try_spawn(&mut storage, &mut stack),
                Err(ThreadSpawnError::ZeroPriority)
            );
        }
    }

    #[test]
    fn rt_thread_builder_try_spawn_reports_null_timer() {
        let mut storage = MaybeUninit::<RtThread>::uninit();
        let mut stack = AlignedStack([0; 64]);

        unsafe {
            assert_eq!(
                RtThreadBuilder::new("bad", test_entry, ptr::null_mut())
                    .try_spawn(&mut storage, &mut stack),
                Err(ThreadSpawnError::NullRtTimer)
            );
        }
    }

    #[test]
    #[should_panic(expected = "CFS thread priority must be non-zero")]
    fn cfs_thread_builder_spawn_panics_on_zero_priority() {
        let mut storage = MaybeUninit::<CfsThread>::uninit();
        let mut stack = AlignedStack([0; 64]);

        unsafe {
            CfsThreadBuilder::new("bad", test_entry, 0).spawn(&mut storage, &mut stack);
        }
    }

    #[test]
    #[should_panic(expected = "thread storage pointer must be non-null")]
    fn raw_forkyi_rejects_null_thread_storage() {
        let mut stack = AlignedStack([0; 64]);

        unsafe {
            forkyi::<CfsThread>(
                ptr::null_mut(),
                stack.top(),
                test_entry,
                ptr::null_mut(),
                "bad",
                1,
            );
        }
    }

    #[test]
    #[should_panic(expected = "thread stack pointer must be non-null")]
    fn raw_forkyi_rejects_null_stack_pointer() {
        let mut storage = MaybeUninit::<CfsThread>::uninit();

        unsafe {
            forkyi(
                storage.as_mut_ptr(),
                ptr::null_mut(),
                test_entry,
                ptr::null_mut(),
                "bad",
                1,
            );
        }
    }

    #[test]
    #[should_panic(expected = "thread stack top must be 8-byte aligned")]
    fn raw_forkyi_rejects_unaligned_stack_top() {
        let mut storage = MaybeUninit::<CfsThread>::uninit();
        let mut stack = AlignedStack([0; 64]);
        let unaligned_sp = unsafe { stack.top().sub(1) };

        unsafe {
            forkyi(
                storage.as_mut_ptr(),
                unaligned_sp,
                test_entry,
                ptr::null_mut(),
                "bad",
                1,
            );
        }
    }

    #[test]
    fn cfs_thread_exposes_sched_info() {
        let mut cfs = cfs_thread("cfs", 3);

        assert!(ptr::eq(cfs.thread_ctx(), &cfs.thread));
        assert_eq!(cfs.sched_info().priority, 3);

        unsafe {
            let handle = thread_handle(&mut cfs.thread);
            let entity = cfs_sched_entity(handle);
            assert!(ptr::eq(
                thread_handle_from_cfs_sched_entity(entity).as_ptr(),
                &cfs.thread
            ));
        }
    }

    #[test]
    fn wait_info_selects_wait_entity_by_thread_class() {
        let mut cfs = cfs_thread("cfs", 1);
        let mut rt = rt_thread("rt");

        cfs.wait_entity.set_wake_after(0, 11);
        cfs.wait_entity.waitevt = 7;
        rt.wait_entity.set_wake_after(0, 13);
        rt.wait_entity.waitevt = 9;

        assert_eq!(cfs.wait_info(), (11, 7));
        assert_eq!(rt.wait_info(), (13, 9));
    }

    #[test]
    fn wait_entity_recovers_thread_context_for_both_thread_classes() {
        let mut cfs = cfs_thread("cfs", 1);
        let mut rt = rt_thread("rt");

        unsafe {
            let cfs_handle = thread_handle(&mut cfs.thread);
            let rt_handle = thread_handle(&mut rt.thread);
            assert!(ptr::eq(
                thread_handle_from_wait_entity(cfs_wait_entity(cfs_handle)).as_ptr(),
                &cfs.thread
            ));
            assert!(ptr::eq(
                thread_handle_from_wait_entity(rt_wait_entity(rt_handle)).as_ptr(),
                &rt.thread
            ));
        }
    }

    #[test]
    fn sync_entity_recovers_thread_context_for_both_thread_classes() {
        let mut cfs = cfs_thread("cfs", 1);
        let mut rt = rt_thread("rt");

        unsafe {
            let cfs_handle = thread_handle(&mut cfs.thread);
            let rt_handle = thread_handle(&mut rt.thread);
            assert!(ptr::eq(
                thread_handle_from_sync_entity(sync_entity(cfs_handle)).as_ptr(),
                &cfs.thread
            ));
            assert!(ptr::eq(
                thread_handle_from_sync_entity(sync_entity(rt_handle)).as_ptr(),
                &rt.thread
            ));
        }
    }

    #[test]
    fn current_thread_helpers_report_missing_current_thread() {
        let _guard = TEST_LOCK.lock().unwrap();

        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }

        assert_eq!(current_thread(), None);
        assert_eq!(current_thread_id(), None);
    }

    #[test]
    fn current_thread_helpers_report_current_cfs_thread() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut cfs = cfs_thread("cfs", 1);

        unsafe {
            CURRENT_THREAD_CTX = &mut cfs.thread;
            CURRENT_THREAD_IS_CFS = true;
        }

        let handle = current_thread().expect("current CFS thread should be set");
        assert_eq!(handle.id(), cfs.thread.id());
        assert_eq!(handle.name(), "cfs");
        assert_eq!(handle.state(), ThreadState::Ready);
        assert!(handle.is_cfs());
        assert_eq!(current_thread_id(), Some(cfs.thread.id()));

        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    #[test]
    fn rt_thread_helpers_access_runtime_and_ktimer_entity() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut rt = rt_thread("rt");
        let mut ktimer = KTimerEntity::new(10);

        assert!(ptr::eq(rt.thread_ctx(), &rt.thread));
        assert!(!rt.has_ktimer());

        unsafe {
            CURRENT_THREAD_CTX = &mut rt.thread;
            CURRENT_THREAD_IS_CFS = false;

            let handle = thread_handle(&mut rt.thread);
            set_rt_ktimer_entity(handle, &mut ktimer);
            assert!(ptr::eq(rt_ktimer_entity(handle), &ktimer));
            assert!(ptr::eq(rt_thread_from_handle(handle), &rt));
        }

        assert!(ptr::eq(rt.ktimer_entity().unwrap().as_ptr(), &ktimer));
        assert!(set_rt_thread_start_time(42));
        assert_eq!(rt.runtime, 42);
        assert_eq!(current_thread_id(), Some(rt.thread.id()));
        assert!(
            current_thread()
                .expect("current RT thread should be set")
                .is_rt()
        );

        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    #[test]
    fn set_rt_thread_start_time_ignores_cfs_current_thread() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut cfs = cfs_thread("cfs", 1);

        unsafe {
            CURRENT_THREAD_CTX = &mut cfs.thread;
            CURRENT_THREAD_IS_CFS = true;
        }

        assert!(!set_rt_thread_start_time(42));

        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }
    }

    #[test]
    fn set_rt_thread_start_time_ignores_missing_current_thread() {
        let _guard = TEST_LOCK.lock().unwrap();

        unsafe {
            CURRENT_THREAD_CTX = ptr::null_mut();
            CURRENT_THREAD_IS_CFS = false;
        }

        assert!(!set_rt_thread_start_time(42));
    }
}
