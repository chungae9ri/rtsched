// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

//! Kernel timer queue keyed by ktimer deadline.
//!
//! The queue is intrusive: each `KTimerEntity` embeds its own `RbNode`, so
//! inserting ktimers does not allocate.

use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::ptr;

use cortex_m::{interrupt, peripheral::SYST};
use rtt_target::rprintln;

use crate::rbtree::{RBTree, RBTreeNode, RbNode};
use crate::runq::{CFS_RUN_QUEUE, update_from_leftmost};
use crate::thread::{
    ThreadCtx, ThreadState, cfs_sched_entity, current_rt_thread_runtime, rt_ktimer_entity,
    rt_thread_from_thread_ctx, set_rt_ktimer_entity,
};
use crate::waitq::{
    WAIT_QUEUE, WaitQueueError, insert_wait_thread, pop_first_wait_thread, remove_wait_thread,
};

pub const CM_SYSTICK_RELOAD_BITS: u32 = 24;
pub const CM_SYSTICK_RELOAD_MAX: u32 = (1 << CM_SYSTICK_RELOAD_BITS) - 1;
static KTIMER_QUEUE: GlobalKTimerQueue = GlobalKTimerQueue::new();
static mut NEXT_KTIMER: *mut KTimerEntity = ptr::null_mut();
pub(crate) static mut CFS_KTIMER: CfsKTimer = CfsKTimer::new(0, 0, "cfs");
pub(crate) static mut WAIT_KTIMER: WaitKTimer = WaitKTimer::inactive();

struct GlobalKTimerQueue {
    queue: UnsafeCell<KTimerQueue>,
}

impl GlobalKTimerQueue {
    const fn new() -> Self {
        Self {
            queue: UnsafeCell::new(KTimerQueue::new()),
        }
    }

    fn get(&self) -> *mut KTimerQueue {
        self.queue.get()
    }
}

unsafe impl Sync for GlobalKTimerQueue {}

#[repr(C)]
pub struct KTimerEntity {
    duration: u32,
    deadline: u32,
    node: RbNode,
    active: bool,
}

impl KTimerEntity {
    pub const fn new(duration: u32) -> Self {
        Self {
            duration,
            deadline: duration,
            node: RbNode::new(),
            active: true,
        }
    }

    pub fn duration(&self) -> u32 {
        self.duration
    }

    pub fn deadline(&self) -> u32 {
        self.deadline
    }

    pub fn set_deadline(&mut self, deadline: u32) {
        self.deadline = deadline;
    }

    pub fn reset_links(&mut self) {
        self.node.reset_links();
    }

    pub fn is_linked(&self) -> bool {
        self.node.is_linked()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    pub unsafe fn rt_ktimer(entity: *mut Self) -> *mut RtKTimer {
        debug_assert!(!entity.is_null());
        debug_assert!(!is_cfs_ktimer(entity));

        (entity as *mut u8)
            .wrapping_sub(offset_of!(RtKTimer, entity))
            .cast::<RtKTimer>()
    }

    pub unsafe fn cfs_ktimer(entity: *mut Self) -> *mut CfsKTimer {
        debug_assert!(!entity.is_null());
        debug_assert!(is_cfs_ktimer(entity));

        (entity as *mut u8)
            .wrapping_sub(offset_of!(CfsKTimer, entity))
            .cast::<CfsKTimer>()
    }
}

#[repr(C)]
pub struct CfsKTimer {
    pub entity: KTimerEntity,
    pub name: &'static str,
    pub execution_ticks: u32,
    pub ktimer_runtime: u32,
}

impl CfsKTimer {
    pub const fn new(duration: u32, execution_ticks: u32, name: &'static str) -> Self {
        Self {
            entity: KTimerEntity::new(duration),
            name,
            execution_ticks,
            ktimer_runtime: 0,
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    pub fn ktimer_runtime(&self) -> u32 {
        self.ktimer_runtime
    }

    pub fn execution_ticks(&self) -> u32 {
        self.execution_ticks
    }

    pub fn add_ktimer_runtime(&mut self, elapsed: u32) {
        self.ktimer_runtime = self.ktimer_runtime.saturating_add(elapsed);
    }

    pub fn reset_ktimer_runtime(&mut self) {
        self.ktimer_runtime = 0;
    }
}

#[repr(C)]
pub struct WaitKTimer {
    pub entity: KTimerEntity,
    pub name: &'static str,
}

impl WaitKTimer {
    pub const fn new(duration: u32, name: &'static str) -> Self {
        Self {
            entity: KTimerEntity::new(duration),
            name,
        }
    }

    pub const fn inactive() -> Self {
        Self {
            entity: KTimerEntity {
                duration: CM_SYSTICK_RELOAD_MAX,
                deadline: CM_SYSTICK_RELOAD_MAX,
                node: RbNode::new(),
                active: false,
            },
            name: "wait",
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    pub fn reset_links(&mut self) {
        self.entity.reset_links();
    }
}

#[repr(C)]
pub struct RtKTimer {
    pub entity: KTimerEntity,
    pub name: &'static str,
    thread_ctx: *mut ThreadCtx,
}

impl RtKTimer {
    pub const fn new(duration: u32, thread_ctx: *mut ThreadCtx, name: &'static str) -> Self {
        Self {
            entity: KTimerEntity::new(duration),
            name,
            thread_ctx,
        }
    }

    pub fn entity_mut(&mut self) -> *mut KTimerEntity {
        ptr::addr_of_mut!(self.entity)
    }

    pub fn thread_ctx(&self) -> *mut ThreadCtx {
        self.thread_ctx
    }

    pub fn init_thread_ctx(&mut self, thread_ctx: *mut ThreadCtx) {
        self.thread_ctx = thread_ctx;
        if !thread_ctx.is_null() {
            unsafe {
                set_rt_ktimer_entity(thread_ctx, self.entity_mut());
            }
        }
    }

    pub fn set_thread_ctx(&mut self, thread_ctx: *mut ThreadCtx) {
        self.init_thread_ctx(thread_ctx);
    }
}

/// Convert a raw tick interval into a SysTick reload register value.
///
/// SysTick reload stores `ticks - 1`, and the register is 24 bits wide.
pub fn reload_from_ticks(ticks: u32) -> Option<u32> {
    ticks
        .checked_sub(1)
        .filter(|&reload| reload <= CM_SYSTICK_RELOAD_MAX)
}

pub unsafe fn init_ktimer_queue() {
    interrupt::free(|_| unsafe {
        ptr::write(KTIMER_QUEUE.get(), KTimerQueue::new());
        ptr::write(&raw mut NEXT_KTIMER, ptr::null_mut());
        ptr::write(&raw mut WAIT_KTIMER, WaitKTimer::inactive());
        (*KTIMER_QUEUE.get()).insert((*ptr::addr_of_mut!(WAIT_KTIMER)).entity_mut());
    });
}

pub unsafe fn enqueue_ktimer(entity: *mut KTimerEntity) {
    interrupt::free(|_| unsafe {
        (*entity).reset_links();
        (*KTIMER_QUEUE.get()).insert(entity);
        refresh_next_ktimer();
    });
}

pub(crate) unsafe fn program_wait_ktimer() {
    interrupt::free(|_| unsafe {
        let wait_entity = (*WAIT_QUEUE.get()).first();

        let wait_ktimer = ptr::addr_of_mut!(WAIT_KTIMER);
        let wait_ktimer_entity = (*wait_ktimer).entity_mut();

        (*KTIMER_QUEUE.get()).remove(wait_ktimer_entity);
        if wait_entity.is_null() {
            (*wait_ktimer_entity).set_deadline(CM_SYSTICK_RELOAD_MAX);
        } else {
            (*wait_ktimer_entity).set_deadline((*wait_entity).wait_ticks);
        }

        (*wait_ktimer_entity).set_active(false); // WAIT_KTIMER is always inactive
        (*KTIMER_QUEUE.get()).insert(wait_ktimer_entity);
        refresh_next_ktimer();
    });
}

pub(crate) unsafe fn wake_wait_thread(elapsed: u32) {
    unsafe {
        let wait_thread = pop_first_wait_thread();
        if !wait_thread.is_null() {
            (*wait_thread).state = ThreadState::Ready;
            if (*wait_thread).is_cfs {
                let entity: *mut crate::runq::SchedEntity = cfs_sched_entity(wait_thread);
                (*entity).reset_links();
                update_from_leftmost(entity);
                (*CFS_RUN_QUEUE.get()).insert(entity);
            } else {
                let ktimer_entity = rt_ktimer_entity(wait_thread);
                if !ktimer_entity.is_null() {
                    (*ktimer_entity)
                        .set_deadline((*ktimer_entity).deadline().saturating_sub(elapsed));
                    (*ktimer_entity).set_active(true);
                    enqueue_ktimer(ktimer_entity);
                }
            }
        }
        program_wait_ktimer();
    }
}

unsafe fn remove_ktimer(entity: *mut KTimerEntity) -> *mut KTimerEntity {
    interrupt::free(|_| unsafe {
        if entity.is_null() {
            return ptr::null_mut();
        }

        let removed = (*KTIMER_QUEUE.get()).remove(entity);
        (*removed).set_active(false);
        if NEXT_KTIMER == removed {
            refresh_next_ktimer();
        }
        removed
    })
}

unsafe fn reinsert_ktimer(entity: *mut KTimerEntity) {
    interrupt::free(|_| unsafe {
        if entity.is_null() {
            return;
        }

        (*entity).set_active(true);
        (*entity).reset_links();
        (*KTIMER_QUEUE.get()).insert(entity);

        refresh_next_ktimer();
    });
}

unsafe fn refresh_next_ktimer() {
    unsafe {
        NEXT_KTIMER = (*KTIMER_QUEUE.get()).first_active();
        if NEXT_KTIMER.is_null() {
            NEXT_KTIMER = activate_cfs_ktimer();
        }
    }
}

pub fn dequeue_ktimerq_to_waitq(thread: *mut ThreadCtx) -> Result<(), WaitQueueError> {
    interrupt::free(|_| unsafe {
        if thread.is_null() || (*thread).is_cfs {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_ktimer(ktimer_entity);
        (*thread).state = ThreadState::Waiting;

        insert_wait_thread(thread);
        program_wait_ktimer();

        Ok(())
    })
}

pub fn enqueue_ktimerq_from_waitq(thread: *mut ThreadCtx) -> Result<(), WaitQueueError> {
    interrupt::free(|_| unsafe {
        if thread.is_null() || (*thread).is_cfs {
            return Err(WaitQueueError::NotFound);
        }

        let ktimer_entity = rt_ktimer_entity(thread);
        if ktimer_entity.is_null() {
            return Err(WaitQueueError::NotFound);
        }

        remove_wait_thread(thread);

        (*thread).state = ThreadState::Ready;
        reinsert_ktimer(ktimer_entity);
        program_wait_ktimer();

        Ok(())
    })
}

pub fn next_ktimer_deadline() -> Option<u32> {
    interrupt::free(|_| unsafe { (*KTIMER_QUEUE.get()).next_deadline() })
}

pub fn next_ktimer_reload() -> Option<u32> {
    interrupt::free(|_| unsafe { (*KTIMER_QUEUE.get()).next_reload() })
}

pub(crate) fn elapsed_ticks_since_last_interrupt() -> u32 {
    SYST::get_reload().saturating_add(1)
}

pub(crate) fn elapsed_ticks_since_current_reload() -> u32 {
    SYST::get_reload().saturating_sub(SYST::get_current())
}

pub(crate) unsafe fn advance_ktimers(elapsed: u32) {
    interrupt::free(|_| unsafe {
        (*KTIMER_QUEUE.get()).advance(elapsed);
    });
}

pub(crate) unsafe fn dispatch_expired_ktimer(elapsed: u32) -> *mut KTimerEntity {
    interrupt::free(|_| unsafe { (*KTIMER_QUEUE.get()).dispatch_expired(elapsed) })
}

pub(crate) unsafe fn update_next_ktimer(entity: *mut KTimerEntity) {
    interrupt::free(|_| unsafe {
        NEXT_KTIMER = entity;
    });
}

pub fn traverse_ktimer_queue() {
    interrupt::free(|_| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();

        rprintln!("ktimer queue:");
        while !entity.is_null() {
            rprintln!(
                "{} ktimer's deadline={}",
                ktimer_name(entity),
                (*entity).deadline()
            );
            entity = queue.next(entity);
        }
    });
}

/// Traverse the ktimer queue and invoke `f` for each ktimer with its name
/// and deadline. This is similar to `traverse_ktimer_queue` but allows the
/// caller to handle formatting/output (for example, writing to UART).
pub fn traverse_ktimer_queue_fn<F>(mut f: F)
where
    F: FnMut(&'static str, u32),
{
    interrupt::free(|_| unsafe {
        let queue = &*KTIMER_QUEUE.get();
        let mut entity = queue.first();

        while !entity.is_null() {
            f(ktimer_name(entity), (*entity).deadline());
            entity = queue.next(entity);
        }
    });
}

pub(crate) fn next_ktimer() -> *mut KTimerEntity {
    interrupt::free(|_| unsafe { NEXT_KTIMER })
}

pub(crate) fn is_cfs_ktimer(entity: *const KTimerEntity) -> bool {
    !entity.is_null() && entity == cfs_ktimer().cast_const()
}

pub(crate) fn is_wait_ktimer(entity: *const KTimerEntity) -> bool {
    !entity.is_null() && entity == wait_ktimer().cast_const()
}

fn cfs_ktimer() -> *mut KTimerEntity {
    unsafe { ptr::addr_of_mut!(CFS_KTIMER.entity) }
}

fn wait_ktimer() -> *mut KTimerEntity {
    unsafe { ptr::addr_of_mut!(WAIT_KTIMER.entity) }
}

#[allow(dead_code)]
unsafe fn ktimer_name(entity: *const KTimerEntity) -> &'static str {
    unsafe {
        if is_cfs_ktimer(entity) {
            (*ptr::addr_of_mut!(CFS_KTIMER)).name
        } else if is_wait_ktimer(entity) {
            (*ptr::addr_of_mut!(WAIT_KTIMER)).name
        } else {
            (*KTimerEntity::rt_ktimer(entity.cast_mut())).name
        }
    }
}

unsafe fn activate_cfs_ktimer() -> *mut KTimerEntity {
    let cfs = cfs_ktimer();
    if !cfs.is_null() {
        unsafe {
            (*cfs).set_active(true);
        }
    }
    cfs
}

pub(crate) unsafe fn yield_ktimer(entity: *mut KTimerEntity, elapsed: u32) -> *mut KTimerEntity {
    interrupt::free(|_| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        if entity.is_null() {
            return ptr::null_mut();
        }

        queue.remove(entity);

        if is_cfs_ktimer(entity) {
            let cfs_timer = ptr::addr_of_mut!(CFS_KTIMER);
            (*cfs_timer).add_ktimer_runtime(elapsed);
            (*entity).set_deadline(
                (*entity)
                    .duration()
                    .saturating_sub((*cfs_timer).ktimer_runtime()),
            );
            (*cfs_timer).reset_ktimer_runtime();
        } else {
            let runtime = current_rt_thread_runtime().unwrap_or(0);
            (*entity).set_deadline((*entity).duration().saturating_sub(runtime));
        }
        (*entity).set_active(false);
        queue.advance(elapsed);
        queue.insert(entity);
        let next = queue.first_active();
        if next.is_null() {
            // TODO:
            // If there no more active timers, select timer
            // for cpu_idle thread.
            activate_cfs_ktimer()
        } else {
            next
        }
    })
}

pub(crate) fn program_next_systick() -> Option<u32> {
    interrupt::free(|_| unsafe {
        let queue = &mut *KTIMER_QUEUE.get();
        let Some(entity) = queue.pop_first() else {
            return None;
        };
        let entity = entity as *mut KTimerEntity;

        let reload = if is_cfs_ktimer(entity) {
            (*KTimerEntity::cfs_ktimer(entity)).execution_ticks()
        } else if is_wait_ktimer(entity) {
            (*entity).deadline()
        } else {
            (*entity).deadline()
        };

        (*entity).set_deadline(reload);
        queue.insert(entity);

        (*SYST::PTR).rvr.write(reload);
        (*SYST::PTR).cvr.write(0);

        Some(reload)
    })
}

unsafe impl RBTreeNode for KTimerEntity {
    fn node(entity: *mut Self) -> *mut RbNode {
        if entity.is_null() {
            ptr::null_mut()
        } else {
            unsafe { ptr::addr_of_mut!((*entity).node) }
        }
    }

    fn entity_of(node: *mut RbNode) -> *mut Self {
        if node.is_null() {
            ptr::null_mut()
        } else {
            unsafe {
                (node as *mut u8)
                    .sub(offset_of!(KTimerEntity, node))
                    .cast::<KTimerEntity>()
            }
        }
    }

    fn entity_of_const(node: *const RbNode) -> *const Self {
        if node.is_null() {
            ptr::null()
        } else {
            unsafe {
                (node as *const u8)
                    .sub(offset_of!(KTimerEntity, node))
                    .cast::<KTimerEntity>()
            }
        }
    }

    unsafe fn cmp(a: *const Self, b: *const Self) -> core::cmp::Ordering {
        unsafe {
            match (*a).deadline.cmp(&(*b).deadline) {
                core::cmp::Ordering::Equal => (a as usize).cmp(&(b as usize)),
                other => other,
            }
        }
    }
}

pub struct KTimerQueue {
    tree: RBTree<KTimerEntity>,
}

impl KTimerQueue {
    pub const fn new() -> Self {
        Self {
            tree: RBTree::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tree.len()
    }

    pub fn root(&self) -> *mut KTimerEntity {
        self.tree.root()
    }

    pub fn first(&self) -> *mut KTimerEntity {
        self.tree.first()
    }

    pub fn last(&self) -> *mut KTimerEntity {
        self.tree.last()
    }

    pub fn next(&self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        self.tree.next(entity)
    }

    pub fn next_deadline(&self) -> Option<u32> {
        let first = self.first();
        if first.is_null() {
            None
        } else {
            Some(unsafe { (*first).deadline() })
        }
    }

    pub fn next_reload(&self) -> Option<u32> {
        self.next_deadline().and_then(reload_from_ticks)
    }

    pub unsafe fn advance(&mut self, elapsed: u32) {
        unsafe {
            let mut advanced = RBTree::new();

            while let Some(entity) = self.pop_first() {
                let entity = entity as *mut KTimerEntity;
                if !(is_wait_ktimer(entity) && (*entity).deadline == CM_SYSTICK_RELOAD_MAX) {
                    (*entity).deadline = (*entity).deadline.saturating_sub(elapsed);
                }
                advanced.insert(entity);
            }

            self.tree = advanced;
        }
    }

    pub unsafe fn dispatch_expired(&mut self, elapsed: u32) -> *mut KTimerEntity {
        unsafe {
            let Some(expired) = self.pop_first() else {
                return ptr::null_mut();
            };

            let expired = expired as *mut KTimerEntity;
            if is_wait_ktimer(expired) {
                //(*expired).deadline = CM_SYSTICK_RELOAD_MAX;
                //(*expired).set_active(false);
                //self.insert(expired);
                wake_wait_thread(elapsed);
            } else if is_cfs_ktimer(expired) {
                let cfs_timer = KTimerEntity::cfs_ktimer(expired);
                (*cfs_timer).add_ktimer_runtime(elapsed);
                (*expired).deadline = (*expired)
                    .duration()
                    .saturating_sub((*cfs_timer).ktimer_runtime());
                (*cfs_timer).reset_ktimer_runtime();
                (*expired).set_active(false);
                self.insert(expired);
            } else {
                // rt_thread missed its deadline, drop and reset its deadline for next round
                let thread_ctx = (*KTimerEntity::rt_ktimer(expired)).thread_ctx();
                let rt_thread = rt_thread_from_thread_ctx(thread_ctx);
                (*rt_thread).runtime = 0;
                (*expired).deadline = (*expired).duration();
                (*expired).set_active(true);
                self.insert(expired);
            }

            let next = self.first_active();
            if next.is_null() {
                // TODO: If no more active timers, run cpu idle thread
                activate_cfs_ktimer()
            } else {
                next
            }
        }
    }

    /// Insert a detached ktimer entity into the queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure `entity` is valid for mutation and is not already
    /// linked into a queue.
    pub unsafe fn insert(&mut self, entity: *mut KTimerEntity) {
        unsafe { self.tree.insert(entity) }
    }

    /// Remove a ktimer entity from the queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure `entity` currently belongs to this queue.
    pub unsafe fn remove(&mut self, entity: *mut KTimerEntity) -> *mut KTimerEntity {
        unsafe { self.tree.remove(entity) }
    }

    /// Remove and return the earliest ktimer entity in the queue.
    pub unsafe fn pop_first(&mut self) -> Option<&mut KTimerEntity> {
        unsafe { self.tree.pop_first() }
    }

    pub fn first_active(&self) -> *mut KTimerEntity {
        let mut entity = self.first();
        while !entity.is_null() {
            if unsafe { (*entity).is_active() } {
                return entity;
            }
            entity = self.next(entity);
        }

        ptr::null_mut()
    }
}

impl Default for KTimerQueue {
    fn default() -> Self {
        Self::new()
    }
}
