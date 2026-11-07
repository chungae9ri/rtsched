#![no_std]
#![no_main]

mod common;

use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cortex_m_rt::{entry, exception};
use rtsched::MessageQueueStatus;

const DEMO_TIMEOUT_MS: u32 = 10;
const SAMPLE_TIMEOUT_MS: u32 = 25;

#[derive(Clone, Copy)]
struct Sample {
    sequence: u32,
    produced_at_cycle: u32,
}

static SAMPLES: rtsched::MessageQueue<Sample, 4> = rtsched::MessageQueue::new();
static PROBE_QUEUE: rtsched::MessageQueue<u32, 1> = rtsched::MessageQueue::new();
// No messages are sent here: first demonstrate timeout, then cancellation.
static CONTROL_QUEUE: rtsched::MessageQueue<u32, 1> = rtsched::MessageQueue::new();
static CANCEL_READY: AtomicBool = AtomicBool::new(false);

static mut IDLE_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut IDLE_THREAD: MaybeUninit<rtsched::IdleThread> = MaybeUninit::uninit();
static mut PRODUCER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut PRODUCER_THREAD: MaybeUninit<rtsched::CfsThread> = MaybeUninit::uninit();
static mut CONSUMER_STACK: rtsched::AlignedStack<{ common::STACK_WORDS }> =
    rtsched::AlignedStack([0; common::STACK_WORDS]);
static mut CONSUMER_THREAD: MaybeUninit<rtsched::RtThread> = MaybeUninit::uninit();
// Initialized before scheduling starts, then only read by the producer.
static mut CONSUMER_HANDLE: MaybeUninit<rtsched::ThreadHandle> = MaybeUninit::uninit();
static mut CONSUMER_TIMER: rtsched::RtKTimer = rtsched::RtKTimer::new_with_timing(
    rtsched::RtTiming::new(
        100 * common::TICKS_PER_MS,
        100 * common::TICKS_PER_MS,
        40 * common::TICKS_PER_MS,
    ),
    core::ptr::null_mut(),
    "consumer",
);

// Inspect these symbols with a debugger. No logging is added to the hot paths.
static PRODUCED: AtomicU32 = AtomicU32::new(0);
static CONSUMED: AtomicU32 = AtomicU32::new(0);
static SEQUENCE_ERRORS: AtomicU32 = AtomicU32::new(0);
static MESSAGE_ERRORS: AtomicU32 = AtomicU32::new(0);
static SEND_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
static RECEIVE_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
static CANCEL_REQUESTS: AtomicU32 = AtomicU32::new(0);
static CANCELLED_RECEIVES: AtomicU32 = AtomicU32::new(0);
static LAST_DELIVERY_CYCLES: AtomicU32 = AtomicU32::new(0);
static PROFILE_DWT_AVAILABLE: AtomicU32 = AtomicU32::new(0);
static PROFILE_PAIR_MIN_CYCLES: AtomicU32 = AtomicU32::new(u32::MAX);
static PROFILE_PAIR_MAX_CYCLES: AtomicU32 = AtomicU32::new(0);
static PROFILE_READ_MIN_CYCLES: AtomicU32 = AtomicU32::new(u32::MAX);

#[entry]
fn main() -> ! {
    let Some(mut peripherals) = cortex_m::Peripherals::take() else {
        common::idle_forever();
    };
    common::init_scheduler(true);
    if rtsched::init_dwt_cycle_counter(&mut peripherals.DCB, &mut peripherals.DWT) {
        PROFILE_DWT_AVAILABLE.store(1, Ordering::Relaxed);
        // Before thread startup, measure only the no-waiter ring path. The
        // pair includes two counter reads; also record their baseline cost.
        cortex_m::interrupt::free(|_| {
            for sequence in 0..256 {
                let start = rtsched::dwt_cycle_count();
                let read_cycles = rtsched::dwt_cycle_count().wrapping_sub(start);
                PROFILE_READ_MIN_CYCLES.fetch_min(read_cycles, Ordering::Relaxed);
                let start = rtsched::dwt_cycle_count();
                PROBE_QUEUE
                    .try_send(core::hint::black_box(sequence))
                    .unwrap();
                core::hint::black_box(PROBE_QUEUE.try_receive().unwrap());
                let cycles = rtsched::dwt_cycle_count().wrapping_sub(start);
                PROFILE_PAIR_MIN_CYCLES.fetch_min(cycles, Ordering::Relaxed);
                PROFILE_PAIR_MAX_CYCLES.fetch_max(cycles, Ordering::Relaxed);
            }
        });
    }
    unsafe {
        let consumer = rtsched::RtThreadBuilder::new(
            "consumer",
            consume,
            core::ptr::addr_of_mut!(CONSUMER_TIMER),
        )
        .spawn(
            core::ptr::addr_of_mut!(CONSUMER_THREAD),
            core::ptr::addr_of_mut!(CONSUMER_STACK),
        );
        core::ptr::addr_of_mut!(CONSUMER_HANDLE).write(MaybeUninit::new(consumer));
        rtsched::CfsThreadBuilder::new("producer", produce, 1)
            .with_arg(core::ptr::addr_of_mut!(CONSUMER_HANDLE).cast())
            .spawn(
                core::ptr::addr_of_mut!(PRODUCER_THREAD),
                core::ptr::addr_of_mut!(PRODUCER_STACK),
            );
        common::configure_systick(&mut peripherals.SYST);
        rtsched::spawn_main_thread(
            "cpu_idle",
            common::cpu_idle,
            core::ptr::addr_of_mut!(IDLE_THREAD),
            core::ptr::addr_of_mut!(IDLE_STACK),
        )
    }
}

extern "C" fn produce(arg: *mut c_void) -> ! {
    // SAFETY: main initialized this static handle before starting the scheduler
    // and never writes it again; the consumer's storage is also static.
    let consumer = unsafe { *arg.cast::<rtsched::ThreadHandle>() };

    // Reuse the now-empty profiling queue. With no receiver, this send must
    // time out and leave the original message intact.
    PROBE_QUEUE.try_send(0).unwrap();
    match PROBE_QUEUE.send_timeout(1, DEMO_TIMEOUT_MS) {
        Err(MessageQueueStatus::TimedOut) => {
            SEND_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            MESSAGE_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }
    if PROBE_QUEUE.try_receive() != Ok(0) {
        MESSAGE_ERRORS.fetch_add(1, Ordering::Relaxed);
    }

    // Wait until the consumer has finished its timeout demo. The flag can
    // precede wait registration, so retry until cancel_wait actually succeeds.
    while !CANCEL_READY.load(Ordering::Acquire) || !CONTROL_QUEUE.cancel_wait(consumer) {
        rtsched::msleepyi(1);
    }
    CANCEL_REQUESTS.fetch_add(1, Ordering::Relaxed);

    let mut sequence = 0_u32;
    loop {
        let sample = Sample {
            sequence,
            produced_at_cycle: rtsched::dwt_cycle_count(),
        };
        match SAMPLES.send_timeout(sample, SAMPLE_TIMEOUT_MS) {
            Ok(()) => {
                PRODUCED.fetch_add(1, Ordering::Relaxed);
                sequence = sequence.wrapping_add(1);
            }
            Err(MessageQueueStatus::TimedOut) => {
                // Nothing was enqueued: retry this sequence on the next loop.
                SEND_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                MESSAGE_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
        }
        rtsched::msleepyi(10);
    }
}

extern "C" fn consume(_arg: *mut c_void) -> ! {
    match CONTROL_QUEUE.receive_timeout(DEMO_TIMEOUT_MS) {
        Err(MessageQueueStatus::TimedOut) => {
            RECEIVE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            MESSAGE_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }

    // The producer cancels this indefinite receive, without ending the thread.
    CANCEL_READY.store(true, Ordering::Release);
    match CONTROL_QUEUE.receive() {
        Err(MessageQueueStatus::Cancelled) => {
            CANCELLED_RECEIVES.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            MESSAGE_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut expected = 0_u32;
    loop {
        match SAMPLES.receive_timeout(SAMPLE_TIMEOUT_MS) {
            Ok(sample) => {
                if sample.sequence != expected {
                    SEQUENCE_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
                expected = sample.sequence.wrapping_add(1);
                LAST_DELIVERY_CYCLES.store(
                    rtsched::dwt_cycle_count().wrapping_sub(sample.produced_at_cycle),
                    Ordering::Relaxed,
                );
                CONSUMED.fetch_add(1, Ordering::Relaxed);
            }
            Err(MessageQueueStatus::TimedOut) => {
                RECEIVE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                MESSAGE_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
        }
        // The producer fills the queue between consumer releases. Its bounded
        // waits can expire; retries preserve the sample sequence.
        rtsched::yieldyi();
    }
}

#[exception]
fn SysTick() {
    rtsched::handle_sched_tick();
}
