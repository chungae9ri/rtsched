use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::ptr;
use std::sync::Mutex;

use rtsched::test_support;
use rtsched::{
    AlignedStack, CfsThread, CfsThreadBuilder, IdleThread, RtKTimer, RtThread, RtThreadBuilder,
    SyncType, ThreadCtx, ThreadKind, ThreadStart, ThreadState, current_thread, current_thread_id,
    init_ktimer_queue, set_rt_thread_start_time,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

extern "C" fn test_entry(_arg: *mut c_void) -> ! {
    loop {
        core::hint::spin_loop();
    }
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
    let mut cfs = test_support::cfs_thread("cfs", 1);

    assert_eq!(cfs.thread_ctx().state, ThreadState::Ready);

    test_support::set_thread_state(cfs.thread_ctx_mut(), ThreadState::Running);
    assert_eq!(cfs.thread_ctx().state, ThreadState::Running);

    test_support::set_thread_state(cfs.thread_ctx_mut(), ThreadState::Waiting);
    assert_eq!(cfs.thread_ctx().state, ThreadState::Waiting);

    test_support::set_thread_state(cfs.thread_ctx_mut(), ThreadState::Ready);
    assert_eq!(cfs.thread_ctx().state, ThreadState::Ready);
}

#[test]
#[should_panic(expected = "invalid thread state transition")]
fn waiting_thread_cannot_transition_directly_to_running() {
    let mut cfs = test_support::cfs_thread("cfs", 1);

    test_support::set_thread_state(cfs.thread_ctx_mut(), ThreadState::Waiting);
    test_support::set_thread_state(cfs.thread_ctx_mut(), ThreadState::Running);
}

#[test]
fn cfs_thread_builder_initializes_typed_storage_and_stack() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mut storage = MaybeUninit::<CfsThread>::uninit();
    let mut stack = AlignedStack([0; 64]);
    let arg = 0x1234usize as *mut c_void;

    unsafe {
        test_support::init_cfs_run_queue();
        let handle = CfsThreadBuilder::new("cfs", test_entry, 3)
            .with_arg(arg)
            .spawn(&mut storage, &mut stack);
        let thread = test_support::thread_ptr(handle);
        let cfs = &*storage.as_ptr();

        assert!(ptr::eq(thread, ptr::addr_of!(*cfs.thread_ctx()).cast_mut()));
        assert_eq!(handle.id().as_u32(), (*thread).id);
        assert_eq!(handle.name(), "cfs");
        assert_eq!(handle.state(), ThreadState::Ready);
        assert!(handle.is_cfs());
        assert!(!handle.is_rt());
        assert!(!handle.is_idle());
        assert_eq!((*thread).name, "cfs");
        assert!((*thread).is_cfs());
        assert_eq!(cfs.sched_info().priority, 3);
        assert_eq!(
            (*thread).sp,
            stack.0.as_mut_ptr().wrapping_add(64 - 16) as usize as u32
        );
        assert_eq!(stack.0[64 - 8], arg as usize as u32);
    }
}

#[test]
fn spawn_idle_thread_initializes_storage_without_cfs_run_queue_priority() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mut storage = MaybeUninit::<IdleThread>::uninit();
    let mut stack = AlignedStack([0; 64]);

    unsafe {
        test_support::init_cfs_run_queue();
        let handle = test_support::spawn_idle_thread_for_test(
            ThreadStart::new("idle", test_entry),
            &mut storage,
            &mut stack,
        );
        let thread = test_support::thread_ptr(handle);
        let idle = &*storage.as_ptr();

        assert!(ptr::eq(
            thread,
            ptr::addr_of!(*idle.thread_ctx()).cast_mut()
        ));
        assert_eq!(handle.name(), "idle");
        assert_eq!(handle.state(), ThreadState::Ready);
        assert!(handle.is_idle());
        assert!(!handle.is_cfs());
        assert!(!handle.is_rt());
        assert_eq!((*thread).kind(), ThreadKind::Idle);
        assert_eq!(test_support::cfs_run_queue_len(), 0);
        assert_eq!(test_support::cfs_run_queue_priority_sum(), 0);
        assert_eq!(
            (*thread).sp,
            stack.0.as_mut_ptr().wrapping_add(64 - 16) as usize as u32
        );
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
        let thread = test_support::thread_ptr(handle);
        let rt = &*storage.as_ptr();

        assert!(ptr::eq(thread, ptr::addr_of!(*rt.thread_ctx()).cast_mut()));
        assert_eq!(handle.id().as_u32(), (*thread).id);
        assert_eq!(handle.name(), "rt");
        assert_eq!(handle.state(), ThreadState::Ready);
        assert!(!handle.is_cfs());
        assert!(handle.is_rt());
        assert!(!handle.is_idle());
        assert_eq!((*thread).name, "rt");
        assert!((*thread).is_rt());
        assert_eq!(rt.runtime(), 0);
        assert!(rt.has_ktimer());
        assert_eq!(
            test_support::rt_ktimer_entity_addr_from_handle(handle),
            test_support::rt_ktimer_entity_addr(&mut ktimer)
        );
        assert_eq!(
            test_support::rt_ktimer_thread_ctx_addr(&ktimer),
            thread as usize
        );
    }
}

#[test]
#[should_panic(
    expected = "thread stack must reserve at least 16 words for the initial platform frame"
)]
fn cfs_thread_builder_rejects_too_small_stack() {
    let mut storage = MaybeUninit::<CfsThread>::uninit();
    let mut stack = AlignedStack([0; test_support::INITIAL_THREAD_FRAME_WORDS - 1]);

    unsafe {
        CfsThreadBuilder::new("bad", test_entry, 1).spawn(&mut storage, &mut stack);
    }
}

#[test]
#[should_panic(expected = "thread storage pointer must be non-null")]
fn cfs_thread_builder_spawn_panics_on_null_thread_storage() {
    let mut stack = AlignedStack([0; 64]);

    unsafe {
        CfsThreadBuilder::new("bad", test_entry, 1).spawn(ptr::null_mut(), &mut stack);
    }
}

#[test]
#[should_panic(expected = "thread stack pointer must be non-null")]
fn cfs_thread_builder_spawn_panics_on_null_stack() {
    let mut storage = MaybeUninit::<CfsThread>::uninit();

    unsafe {
        CfsThreadBuilder::new("bad", test_entry, 1)
            .spawn(&mut storage, ptr::null_mut::<AlignedStack<64>>());
    }
}

#[test]
#[should_panic(expected = "thread stack top must be 8-byte aligned")]
fn cfs_thread_builder_spawn_panics_on_unaligned_stack_top() {
    let mut storage = MaybeUninit::<CfsThread>::uninit();
    let mut odd_stack = AlignedStack([0; test_support::INITIAL_THREAD_FRAME_WORDS + 1]);

    unsafe {
        CfsThreadBuilder::new("bad", test_entry, 1).spawn(&mut storage, &mut odd_stack);
    }
}

#[test]
#[should_panic(expected = "RT thread ktimer must be non-null")]
fn rt_thread_builder_spawn_panics_on_null_timer() {
    let mut storage = MaybeUninit::<RtThread>::uninit();
    let mut stack = AlignedStack([0; 64]);

    unsafe {
        RtThreadBuilder::new("bad", test_entry, ptr::null_mut()).spawn(&mut storage, &mut stack);
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
        test_support::raw_forkyi_cfs(
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
        test_support::raw_forkyi_cfs(
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
        test_support::raw_forkyi_cfs(
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
    let mut cfs = test_support::cfs_thread("cfs", 3);

    assert_eq!(cfs.thread_ctx().name(), "cfs");
    assert!(cfs.thread_ctx().is_cfs());
    assert_eq!(cfs.sched_info().priority, 3);

    unsafe {
        assert_eq!(
            test_support::cfs_sched_entity_owner_addr(&mut cfs),
            cfs.thread_ctx() as *const ThreadCtx as usize
        );
    }
}

#[test]
fn wait_info_selects_wait_entity_by_thread_class() {
    let mut cfs = test_support::cfs_thread("cfs", 1);
    let mut rt = test_support::rt_thread("rt");

    test_support::set_cfs_wait_info(&mut cfs, 11, Some(SyncType::BinarySemaphore));
    test_support::set_rt_wait_info(&mut rt, 13, Some(SyncType::Mutex));

    assert_eq!(cfs.wait_info(), (11, Some(SyncType::BinarySemaphore)));
    assert_eq!(rt.wait_info(), (13, Some(SyncType::Mutex)));
}

#[test]
fn wait_entity_recovers_thread_context_for_both_thread_classes() {
    let mut cfs = test_support::cfs_thread("cfs", 1);
    let mut rt = test_support::rt_thread("rt");

    unsafe {
        assert_eq!(
            test_support::cfs_wait_entity_owner_addr(&mut cfs),
            cfs.thread_ctx() as *const ThreadCtx as usize
        );
        assert_eq!(
            test_support::rt_wait_entity_owner_addr(&mut rt),
            rt.thread_ctx() as *const ThreadCtx as usize
        );
    }
}

#[test]
fn sync_entity_recovers_thread_context_for_both_thread_classes() {
    let mut cfs = test_support::cfs_thread("cfs", 1);
    let mut rt = test_support::rt_thread("rt");

    unsafe {
        assert_eq!(
            test_support::cfs_sync_entity_owner_addr(&mut cfs),
            cfs.thread_ctx() as *const ThreadCtx as usize
        );
        assert_eq!(
            test_support::rt_sync_entity_owner_addr(&mut rt),
            rt.thread_ctx() as *const ThreadCtx as usize
        );
        assert_eq!(
            test_support::sync_entity_owner_addr(cfs.thread_ctx_mut()),
            cfs.thread_ctx() as *const ThreadCtx as usize
        );
        assert_eq!(
            test_support::sync_entity_owner_addr(rt.thread_ctx_mut()),
            rt.thread_ctx() as *const ThreadCtx as usize
        );
    }
}

#[test]
fn current_thread_helpers_report_missing_current_thread() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        test_support::clear_current_thread();
    }

    assert_eq!(current_thread(), None);
    assert_eq!(current_thread_id(), None);
}

#[test]
fn current_thread_helpers_report_current_cfs_thread() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mut cfs = test_support::cfs_thread("cfs", 1);

    unsafe {
        test_support::set_current_thread(cfs.thread_ctx_mut(), true);
    }

    let handle = current_thread().expect("current CFS thread should be set");
    assert_eq!(handle.id(), cfs.thread_ctx().id());
    assert_eq!(handle.name(), "cfs");
    assert_eq!(handle.state(), ThreadState::Ready);
    assert!(handle.is_cfs());
    assert_eq!(current_thread_id(), Some(cfs.thread_ctx().id()));

    unsafe {
        test_support::clear_current_thread();
    }
}

#[test]
fn rt_thread_helpers_access_runtime_and_ktimer_entity() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mut rt = test_support::rt_thread("rt");
    let mut ktimer = RtKTimer::new(10, ptr::null_mut(), "rt");

    assert_eq!(rt.thread_ctx().name(), "rt");
    assert!(rt.thread_ctx().is_rt());
    assert!(!rt.has_ktimer());

    unsafe {
        test_support::set_current_thread(rt.thread_ctx_mut(), false);

        let handle = test_support::attach_rt_ktimer(&mut rt, &mut ktimer);
        assert_eq!(
            test_support::rt_ktimer_entity_addr_from_handle(handle),
            test_support::rt_ktimer_entity_addr(&mut ktimer)
        );
        assert_eq!(
            test_support::rt_thread_addr_from_handle(handle),
            &rt as *const RtThread as usize
        );
    }

    assert!(rt.has_ktimer());
    assert!(set_rt_thread_start_time(42));
    assert_eq!(rt.runtime(), 42);
    assert_eq!(current_thread_id(), Some(rt.thread_ctx().id()));
    assert!(
        current_thread()
            .expect("current RT thread should be set")
            .is_rt()
    );

    unsafe {
        test_support::clear_current_thread();
    }
}

#[test]
fn set_rt_thread_start_time_ignores_cfs_current_thread() {
    let _guard = TEST_LOCK.lock().unwrap();
    let mut cfs = test_support::cfs_thread("cfs", 1);

    unsafe {
        test_support::set_current_thread(cfs.thread_ctx_mut(), true);
    }

    assert!(!set_rt_thread_start_time(42));

    unsafe {
        test_support::clear_current_thread();
    }
}

#[test]
fn set_rt_thread_start_time_ignores_missing_current_thread() {
    let _guard = TEST_LOCK.lock().unwrap();

    unsafe {
        test_support::clear_current_thread();
    }

    assert!(!set_rt_thread_start_time(42));
}
