// SPDX-License-Identifier: MIT
// Copyright (c) 2026 kwangdo.yi

#[cfg(target_arch = "arm")]
mod imp {
    use core::arch::{asm, global_asm};
    use core::ptr;

    use crate::runq::CFS_RUN_QUEUE;
    use crate::sched::{CURRENT_THREAD_IS_CFS, is_idle_thread, reset_scheduler_started};
    use crate::thread::{ThreadCtx, ThreadHandle, ThreadState, cfs_sched_entity};

    #[unsafe(no_mangle)]
    static mut START_THREAD_PTR: *mut ThreadCtx = ptr::null_mut();

    /// Spawn main thread by restoring its prepared stack frame.
    ///
    /// This does not return. The thread must already have been initialized with the
    /// same synthetic frame layout produced by `forkyi`. The actual exception
    /// return happens in `SVCall`, because `EXC_RETURN` is only valid from
    /// handler mode.
    ///
    /// # Safety
    ///
    /// `thread` must refer to a live thread initialized by `forkyi` or a thread
    /// builder, and its stack storage must remain valid for the lifetime of the
    /// running thread. Call this only once the scheduler queues have been
    /// initialized and no other thread is currently running.
    pub unsafe fn spawn_main_thread(thread: ThreadHandle) -> ! {
        unsafe {
            let thread_ptr = thread.as_ptr();
            if !is_idle_thread(thread_ptr) {
                (*CFS_RUN_QUEUE.get()).remove(cfs_sched_entity(thread));
            }
            (*thread_ptr).set_state(ThreadState::Running);
            reset_scheduler_started();
            START_THREAD_PTR = thread_ptr;
            CURRENT_THREAD_IS_CFS = true;
            asm!("svc 0", options(noreturn));
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
    // - `str r0, [r2]`    -> stores saved SP into `ThreadCtx.sp` (offset 0)
    // - `str lr, [r2, #4]`-> stores EXC_RETURN into `ThreadCtx.exc_return` (offset 4)
    global_asm!(
        ".section .text.SVCall,\"ax\",%progbits",
        ".global SVCall",
        ".type SVCall,%function",
        "SVCall:",
        "ldr r0, =START_THREAD_PTR",
        "ldr r0, [r0]", // r0 = thread
        "ldr r3, =CURRENT_THREAD_CTX",
        "str r0, [r3]", // CURRENT_THREAD_CTX = thread
        "dmb sy",
        "ldr r3, =SCHEDULER_STARTED",
        "movs r2, #1",
        "str r2, [r3]",
        "ldr r3, =CURRENT_THREAD_CTX",
        "ldr r0, [r3]",          // r0 = thread
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

    macro_rules! pendsv_handler {
        ($($sched_isr_timing:literal,)*) => {
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
                "str lr, [r2, #4]", // Save EXC_RETURN so the next restore uses MSP or PSP correctly.
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
                $($sched_isr_timing,)*
                "bx lr",
            );
        };
    }

    #[cfg(feature = "sched-isr-timing")]
    pendsv_handler!(
        "ldr r1, =SCHED_TICK_TO_PENDSV_ARMED",
        "ldr r2, [r1]",
        "cbz r2, 1f",
        "movs r2, #0",
        "str r2, [r1]",
        "ldr r1, =0xE0001004", // DWT CYCCNT
        "ldr r3, [r1]",
        "ldr r1, =SCHED_TICK_TO_PENDSV_START_CYCLE",
        "ldr r2, [r1]",
        "subs r3, r3, r2",
        "ldr r1, =SCHED_TICK_TO_PENDSV_LAST_TICKS",
        "str r3, [r1]",
        "ldr r1, =SCHED_TICK_TO_PENDSV_SAMPLES",
        "ldr r2, [r1]",
        "adds r2, r2, #1",
        "str r2, [r1]",
        "ldr r1, =SCHED_TICK_TO_PENDSV_MAX_TICKS",
        "ldr r2, [r1]",
        "cmp r2, r3",
        "it lo",
        "strlo r3, [r1]",
        "1:",
    );

    #[cfg(not(feature = "sched-isr-timing"))]
    pendsv_handler!();
}

#[cfg(target_arch = "arm")]
pub use self::imp::spawn_main_thread;
