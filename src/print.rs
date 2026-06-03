use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

type PrintFn = fn(&str);

static PRINT_FN: AtomicUsize = AtomicUsize::new(0);

pub fn set_print_fn(print_fn: PrintFn) {
    PRINT_FN.store(print_fn as usize, Ordering::Release);
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    struct Printer;

    impl Write for Printer {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let print_fn = PRINT_FN.load(Ordering::Acquire);
            if print_fn != 0 {
                let print_fn: PrintFn = unsafe { core::mem::transmute(print_fn) };
                print_fn(s);
            }
            Ok(())
        }
    }

    let _ = Printer.write_fmt(args);
}

#[macro_export]
macro_rules! rtsched_println {
    () => {
        $crate::print::_print(core::format_args!("\r\n"))
    };
    ($($arg:tt)*) => {{
        $crate::print::_print(core::format_args!($($arg)*));
        $crate::print::_print(core::format_args!("\r\n"));
    }};
}
