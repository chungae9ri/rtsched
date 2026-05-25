// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

use core::arch::global_asm;
use core::ptr;

use core::arch::asm;
use cortex_m::peripheral::SCB;

use crate::ktimer::{
    CfsKTimer, KTimerEntity, advance_ktimers, dispatch_expired_ktimer,
    elapsed_ticks_since_last_interrupt, enqueue_ktimer, is_cfs_ktimer, is_wait_ktimer, next_ktimer,
    program_next_systick, update_next_ktimer,
};
use crate::runq::{CFS_RUN_QUEUE, SchedEntity, init_cfs_rq};
use crate::thread::{
    ThreadCtx, ThreadState, cfs_sched_entity, thread_from_cfs_sched_entity, yieldyi_with_elapsed,
};

pub(crate) static mut CFS_KTIMER: CfsKTimer = CfsKTimer::new(0, 0, "cfs");
#[unsafe(no_mangle)]
pub static mut CURRENT_THREAD_CTX: *mut ThreadCtx = ptr::null_mut();
pub(crate) static mut CURRENT_THREAD_IS_CFS: bool = false;
#[unsafe(no_mangle)]
static mut START_THREAD_PTR: *mut ThreadCtx = ptr::null_mut();

/// Spawn main thread by restoring its prepared stack frame.
///
/// This does not return. The thread must already have been initialized with the
/// same synthetic frame layout produced by `forkyi`. The actual exception
/// return happens in `SVCall`, because `EXC_RETURN` is only valid from
/// handler mode.
pub unsafe fn spawn_main_thread(thread: *mut ThreadCtx) -> ! {
    unsafe {
        (*CFS_RUN_QUEUE.get()).remove(cfs_sched_entity(thread));
        (*thread).state = ThreadState::Running;
        START_THREAD_PTR = thread;
        CURRENT_THREAD_IS_CFS = true;
        asm!("svc 0", options(noreturn));
    }
}

/// Initialize the CFS scheduler state and enqueue its scheduler timer.
///
/// `ticks` is expressed in raw timer ticks because the board owns the clock
/// configuration.
pub unsafe fn init_cfs(period_ticks: u32, exec_ticks: u32) {
    unsafe {
        init_cfs_rq();
        CFS_KTIMER = CfsKTimer::new(period_ticks, exec_ticks, "cfs");
        let cfs_ktimer = (*ptr::addr_of_mut!(CFS_KTIMER)).entity_mut();
        (*cfs_ktimer).set_deadline(exec_ticks);
        enqueue_ktimer(cfs_ktimer);
    }
}

// Switch to the first thread which was set up by `forkyi`.
// This is typically called at the end of `main`.
// NOTE: Assembly below relies on the `ThreadCtx` layout defined in
// `rtsched/src/thread.rs` where `ThreadCtx.sp` is the first field (offset 0)
// and `ThreadCtx.exc_return` is the second field (offset 4). The save/restore
// sequence performed by PendSV/SVCall pushes r4-r11 and, when EXC_RETURN bit 4
// indicates an active FP context, s16-s31 onto the thread's stack and stores
// the stack pointer into `ThreadCtx.sp`.
//
// Stack frame expectations produced by `forkyi`:
// - The synthetic thread entry frame left for exception return contains
//   (from low to high addresses): r4..r11 (pushed by PendSV), then the
//   standard hardware frame consumed by EXC_RETURN: r0, r1, r2, r3, r12, lr,
//   pc, xPSR. `ThreadCtx.sp` points at the saved r4..r11 block (the full saved
//   context begins at this pointer when restoring).
// - Threads that use the FPU also carry an extended hardware exception frame
//   for s0-s15/FPSCR and a software-saved s16-s31 block immediately above the
//   r4-r11 block. EXC_RETURN bit 4 selects whether the s16-s31 block is present.
//
// Offsets used by the assembly:
// - `str r0, [r2]`   -> stores saved SP into `ThreadCtx.sp` (offset 0)
// - `str lr, [r2, #4]`-> stores EXC_RETURN into `ThreadCtx.exc_return` (offset 4)
global_asm!(
    ".section .text.SVCall,\"ax\",%progbits",
    ".global SVCall",
    ".type SVCall,%function",
    "SVCall:",
    "ldr r0, =START_THREAD_PTR",
    "ldr r0, [r0]", // r0 = thread
    "ldr r3, =CURRENT_THREAD_CTX",
    "str r0, [r3]",          // CURRENT_THREAD_CTX = thread
    "ldr r1, [r0]",          // r1 = thread->sp
    "ldr lr, [r0, #4]",      // lr = thread->exc_return
    "ldmia r1!, {{r4-r11}}", // restore callee-saved registers
    "tst lr, #4",
    "ite eq",
    "msreq msp, r1",
    "msrne psp, r1",
    "bx lr", // exception return into the thread entry frame
    ".size SVCall, .-SVCall",
);

// PendSV handler used for context switching between threads.
// The actual context switch happens in the assembly code, but the scheduler is
// called from here to select the next thread to run and update `CURRENT_THREAD_CTX`.
// Threads are expected to have their stack frames (PSP) prepared by `forkyi` so that the
// assembly code can save and restore them without needing to understand the layout.
global_asm!(
    ".section .text.PendSV,\"ax\",%progbits",
    ".global PendSV",
    ".type PendSV,%function",
    "PendSV:",
    "tst lr, #4", // Was the interrupted thread using PSP or MSP
    "ite eq",
    "mrseq r0, msp", // Thread used MSP.
    "mrsne r0, psp", // Thread used PSP.
    "tst lr, #0x10", // EXC_RETURN bit 4 clear means an FP context is active.
    "it eq",
    "vstmdbeq r0!, {{s16-s31}}", // Save callee-saved FP registers when present.
    "stmdb r0!, {{r4-r11}}",     // Save callee-saved core registers on the thread stack.
    "ldr r1, =CURRENT_THREAD_CTX", // R1 = &CURRENT_THREAD_CTX
    "ldr r2, [r1]",              // R2 = CURRENT_THREAD_CTX thread pointer
    "str r0, [r2]",              // Save updated stack pointer into the thread control block.
    "str lr, [r2, #4]",          // Save EXC_RETURN so the next restore uses MSP or PSP correctly.
    "bl schedule", // Run the CURRENT_THREAD_CTX ktimer handler and update CURRENT_THREAD_CTX.
    "ldr r1, =CURRENT_THREAD_CTX", // R1 = &CURRENT_THREAD_CTX
    "ldr r2, [r1]", // R2 = next thread pointer
    "ldr r0, [r2]", // R0 = next thread's saved SP
    "ldr lr, [r2, #4]", // LR = next thread's saved EXC_RETURN
    "ldmia r0!, {{r4-r11}}", // Restore callee-saved core registers for the selected thread.
    "tst lr, #0x10", // EXC_RETURN bit 4 clear means an FP context is active.
    "it eq",
    "vldmiaeq r0!, {{s16-s31}}", // Restore callee-saved FP registers when present.
    "tst lr, #4",                // Does the next thread return using MSP or PSP?
    "ite eq",
    "msreq msp, r0", // Restore MSP-backed context.
    "msrne psp, r0", // Restore PSP-backed context.
    "bx lr",
);

#[unsafe(no_mangle)]
extern "C" fn schedule() {
    unsafe {
        let next_ktimer = next_ktimer();
        if next_ktimer.is_null() {
            program_next_systick();
            return;
        }

        // The scheduler logic is as follows:
        // - If the CURRENT_THREAD_CTX is CFS, update its vruntime based on the elapsed
        //   ticks and its priority.
        // - If the next expired ktimer is for a CFS thread and current thread is
        //   CFS thread, compare its vruntime with the CURRENT_THREAD_CTX's vruntime
        //   to decide whether to preempt.
        // - If the next expired ktimer is for a CFS thread and current thread is
        //   RT thread, switch to the left-most CFS thread.
        // - If the next expired ktimer is for an RT thread and current thread is
        //   CFS thread, insert current to CFS runq and switch to next RT thread.
        // - If the next expired ktimer is for an RT thread and current thread is
        //   RT thread,preempt the CURRENT_THREAD_CTX with next RT thread.
        if CURRENT_THREAD_IS_CFS && (*CURRENT_THREAD_CTX).state == ThreadState::Running {
            let current_entity = cfs_sched_entity(CURRENT_THREAD_CTX);
            (*current_entity).sched_tick_cnt += u64::from(elapsed_ticks_since_last_interrupt());
            let priority_sum = *CFS_RUN_QUEUE.priority_sum();
            if priority_sum == 0 {
                return;
            }
            let sched_tick_cnt = (*current_entity).sched_tick_cnt;
            let priority = u64::from((*current_entity).priority);
            let priority_sum = u64::from(priority_sum);

            (*current_entity).vruntime = sched_tick_cnt * priority / priority_sum;
        }

        if is_cfs_ktimer(next_ktimer) {
            if let Some(next_entity) = (*CFS_RUN_QUEUE.get()).pop_first() {
                let next_thread = thread_from_cfs_sched_entity(next_entity as *mut SchedEntity);

                if CURRENT_THREAD_IS_CFS {
                    if (*CURRENT_THREAD_CTX).state == ThreadState::Waiting {
                        (*next_thread).state = ThreadState::Running;
                        CURRENT_THREAD_CTX = next_thread;
                        CURRENT_THREAD_IS_CFS = true;
                    } else {
                        let current_entity = cfs_sched_entity(CURRENT_THREAD_CTX);
                        debug_assert!(
                            CURRENT_THREAD_CTX != next_thread,
                            "CFS_RUN_QUEUE.pop_first() returned the CURRENT_THREAD_CTX running thread"
                        );
                        if (*current_entity).vruntime > (*next_entity).vruntime {
                            (*CURRENT_THREAD_CTX).state = ThreadState::Ready;
                            (*CFS_RUN_QUEUE.get()).insert(current_entity);
                            (*next_thread).state = ThreadState::Running;
                            CURRENT_THREAD_CTX = next_thread;
                            CURRENT_THREAD_IS_CFS = true;
                        } else {
                            (*CFS_RUN_QUEUE.get()).insert(next_entity as *mut SchedEntity);
                        }
                    }
                } else {
                    if (*CURRENT_THREAD_CTX).state != ThreadState::Waiting {
                        (*CURRENT_THREAD_CTX).state = ThreadState::Ready;
                    }
                    (*next_thread).state = ThreadState::Running;
                    CURRENT_THREAD_CTX = next_thread;
                    CURRENT_THREAD_IS_CFS = true;
                }
            }
        } else if is_wait_ktimer(next_ktimer) {
            // TODO: If next_ktimer is a wait ktimer, pick next active ktimer (rt or cfs).
            //       If there is no active ktimer (rt or cfs) to run, run cpu_idle thread.
            //       For now, we just run CFS KTimer thread.
            if let Some(next_entity) = (*CFS_RUN_QUEUE.get()).pop_first() {
                let next_thread = thread_from_cfs_sched_entity(next_entity as *mut SchedEntity);

                (*next_thread).state = ThreadState::Running;
                CURRENT_THREAD_CTX = next_thread;
                CURRENT_THREAD_IS_CFS = true;
            }
        } else {
            let next_thread = (*KTimerEntity::rt_ktimer(next_ktimer)).thread_ctx();

            if next_thread.is_null() {
                return;
            }

            if (*CURRENT_THREAD_CTX).state != ThreadState::Waiting {
                (*CURRENT_THREAD_CTX).state = ThreadState::Ready;
                if CURRENT_THREAD_IS_CFS {
                    (*CFS_RUN_QUEUE.get()).insert(cfs_sched_entity(CURRENT_THREAD_CTX));
                }
            }
            (*next_thread).state = ThreadState::Running;
            CURRENT_THREAD_CTX = next_thread;
            CURRENT_THREAD_IS_CFS = false;
        }

        program_next_systick();
    }
}

/// Handle one SysTick event and request ktimer dispatch.
pub fn handle_systick() {
    let elapsed = elapsed_ticks_since_last_interrupt();

    let next_ktimer = unsafe {
        advance_ktimers(elapsed);
        dispatch_expired_ktimer(elapsed)
    };

    unsafe {
        if CURRENT_THREAD_IS_CFS {
            let cfs_timer = ptr::addr_of_mut!(CFS_KTIMER);
            (*cfs_timer).add_ktimer_runtime(elapsed);
            if (*cfs_timer).ktimer_runtime() >= (*cfs_timer).execution_ticks() {
                (*cfs_timer).reset_ktimer_runtime();
                // elapsed time is already counted in advance_ktimers.
                yieldyi_with_elapsed(0);
                return;
            }
        }
    }

    // This should be done before updating KTIMER_QUEUE
    //let is_current_wait_ktimer = ktimer_queue_leftmost_is_wait_ktimer();
    unsafe {
        if !next_ktimer.is_null() && !is_wait_ktimer(next_ktimer) {
            (*next_ktimer).set_active(true);
        }

        update_next_ktimer(next_ktimer);
    }

    if !next_ktimer.is_null() {
        SCB::set_pendsv();
    }
}
