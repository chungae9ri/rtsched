use std::string::String;
use std::sync::Mutex;
use std::vec::Vec;

use rtsched::print::_print;
use rtsched::set_print_fn;
use rtsched::test_support;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static PRINTED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn capture_print(s: &str) {
    PRINTED.lock().unwrap().push(String::from(s));
}

fn reset_capture() {
    PRINTED.lock().unwrap().clear();
    test_support::clear_print_fn_for_test();
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

    rtsched::rtsched_println!("tick {}", 7);

    assert_eq!(PRINTED.lock().unwrap().as_slice(), ["tick 7", "\r\n"]);
}
