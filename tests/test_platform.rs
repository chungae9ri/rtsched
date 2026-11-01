use core::ffi::c_void;

use rtsched::{AlignedStack, DefaultPlatform, ThreadStackPort};

extern "C" fn test_entry(_arg: *mut c_void) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[test]
fn host_stack_initializes_embedded_thread_frame_layout() {
    let mut stack = AlignedStack([0xa5a5_a5a5; 64]);
    let arg = 0x1234usize as *mut c_void;

    let context = unsafe { DefaultPlatform::init_thread_stack(stack.top(), test_entry, arg) };
    let frame_start = 64 - DefaultPlatform::INITIAL_FRAME_WORDS;

    assert_eq!(
        context.sp,
        stack.0.as_ptr().wrapping_add(frame_start) as usize as u32
    );
    assert_eq!(context.exc_return, 0xFFFF_FFFD);
    assert!(
        stack.0[..frame_start]
            .iter()
            .all(|word| *word == 0xa5a5_a5a5)
    );

    // Saved r4-r11, followed by the r0-r3/r12/lr/pc/xPSR exception frame.
    assert_eq!(
        &stack.0[frame_start..],
        &[
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            arg as usize as u32,
            0,
            0,
            0,
            0,
            0xFFFF_FFFD,
            test_entry as *const () as usize as u32,
            0x0100_0000,
        ]
    );
}
