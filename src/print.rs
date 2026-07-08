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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use std::string::String;
    use std::sync::Mutex;
    use std::vec::Vec;

    static PRINTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn capture_print(s: &str) {
        PRINTED.lock().unwrap().push(String::from(s));
    }

    fn reset_capture() {
        PRINTED.lock().unwrap().clear();
        PRINT_FN.store(0, Ordering::Release);
    }

    #[test]
    fn print_without_callback_is_ignored() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_capture();

        _print(format_args!("hello"));

        assert!(PRINTED.lock().unwrap().is_empty());
    }

    #[test]
    fn print_uses_registered_callback() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_capture();
        set_print_fn(capture_print);

        _print(format_args!("{} {}", "hello", 42));

        assert_eq!(PRINTED.lock().unwrap().as_slice(), ["hello 42"]);
    }

    #[test]
    fn println_macro_prints_message_and_crlf() {
        let _guard = TEST_LOCK.lock().unwrap();

        reset_capture();
        set_print_fn(capture_print);

        crate::rtsched_println!("tick {}", 7);

        assert_eq!(PRINTED.lock().unwrap().as_slice(), ["tick 7", "\r\n"]);
    }
}
