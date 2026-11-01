# Message queues

[Documentation index](../README.md) · Source: [`src/message.rs`](../src/message.rs)

`MessageQueue<T, N>` transfers FIFO messages between any number of CFS and RT
threads on the scheduler core. It uses fixed inline storage, has no heap
dependency, and supports both backpressure and nonblocking interrupt-to-thread
delivery.

## Quick start

Use a shared `MessageQueue<T, N>` for bounded FIFO delivery between CFS and RT
threads. Messages are copied into inline storage, so no heap is required.

```rust
static EVENTS: rtsched::MessageQueue<u32, 8> = rtsched::MessageQueue::new();

// Producer thread: suspend while the queue is full.
EVENTS.send(42).unwrap();
// Consumer thread: suspend while the queue is empty.
let event = EVENTS.receive().unwrap();
```

`try_send` and `try_receive` return `MessageQueueStatus::Full` or
`MessageQueueStatus::Empty` immediately and can be used in ordinary maskable
interrupt handlers on the scheduler core. Payloads must be
`Copy`, and shared queues additionally require `Send`. Blocking waits use
`Suspended`; the earliest-deadline opposite waiter is woken and retries the
operation. There are no timeouts or close operations in this version.

## API and usage

```rust
use rtsched::{MessageQueue, MessageQueueStatus};

#[derive(Clone, Copy)]
struct Reading {
    sequence: u32,
    value: i32,
}

static READINGS: MessageQueue<Reading, 8> = MessageQueue::new();

// Producer thread: sleeps if all eight slots are occupied.
fn publish(sequence: u32, value: i32) -> Result<(), MessageQueueStatus> {
    READINGS.send(Reading { sequence, value })
}

// Consumer thread: sleeps if there is no reading yet.
fn next_reading() -> Result<Reading, MessageQueueStatus> {
    READINGS.receive()
}
```

| Operation | Success | Unavailable queue |
| --- | --- | --- |
| `try_send(value)` | Copies the value into the next FIFO slot; wakes one receiver | Returns `Full`; changes nothing |
| `try_receive()` | Returns the oldest value; wakes one sender | Returns `Empty`; changes nothing |
| `send(value)` | Same enqueue as `try_send` | Suspends the running thread and retries when woken |
| `receive()` | Same dequeue as `try_receive` | Suspends the running thread and retries when woken |
| `capacity()` | Compile-time capacity `N` | Never blocks |
| `len()`, `is_empty()`, `is_full()` | Instantaneous buffer occupancy | Never suspends; snapshots can immediately become stale |

Send and receive operations return `Result<_, MessageQueueStatus>`. Successful
operations return `Ok`; `Err(MessageQueueStatus::Full)` and
`Err(MessageQueueStatus::Empty)` are normal, retryable outcomes of nonblocking
operations. Blocking operations handle full/empty conditions by suspending and
retrying. The other status variants report an unavailable execution context or
a scheduler wait-queue failure.

`new()` and `Default` create an empty queue. `new()` is `const`, so a queue can
be declared as a safe shared `static`. Zero capacity is a setup error and panics
(or fails constant evaluation for a static). Every slot is usable: capacity is
exactly `N`, including `N == 1`.

Messages must implement `Copy`. Sharing or transferring a queue between threads
requires `T: Send`; `T: Sync` is unnecessary because the API never lends references
into its storage. A caller retains its original message on a send error. Prefer
small values such as tagged enums, numeric samples, or indexes into an
application-owned buffer pool. A copied reference must still obey Rust's lifetime
and sharing rules; the queue does not make a borrowed buffer live longer.

There is no silent overwrite or drop-on-full policy. An interrupt handler can
explicitly decide how to handle `Full`:

```rust
# use rtsched::{MessageQueue, MessageQueueStatus};
# static EVENTS: MessageQueue<u32, 8> = MessageQueue::new();
fn on_device_event(value: u32) {
    match EVENTS.try_send(value) {
        Ok(()) => {}
        Err(MessageQueueStatus::Full) => {
            // Record an application overflow counter or apply device flow control.
        }
        Err(_) => unreachable!(),
    }
}
```

Only ordinary maskable interrupt handlers on the scheduler core may use the
nonblocking API. NMI, HardFault, other cores, and DMA writing directly into queue
storage are outside the platform critical section's protection. A nonblocking
operation can still perform scheduler wakeup work before returning.

Blocking calls require a selected, `Running`, non-idle thread. If an unavailable
queue has no selected thread, they return `NoCurrentThread`. If it cannot legally
block, they return `InvalidContext` without registering a waiter. Cortex-M checks
thread mode and the caller's PRIMASK, BASEPRI, and FAULTMASK before entering its
own critical section. Leave PendSV enabled and do not call blocking operations
from another critical section. Calls that can complete immediately succeed
without a running scheduler, including native host builds. Native builds return
an error rather than trying to park a host OS thread when blocking is required.

This first version has indefinite waits only. It has no timeout, cancellation,
close/disconnect, dynamic resizing, broadcast, or priority inheritance. RT code
with a strict completion bound should use `try_send`/`try_receive` and an explicit
application fallback. A full queue can otherwise depend indefinitely on a slow
consumer, and an empty queue on a slow producer.

## Storage and invariants

The implementation is in `src/message.rs`. Its `UnsafeCell<QueueState<T, N>>` contains:

- `[MaybeUninit<T>; N]` for the payloads;
- a head index and a count of initialized, occupied slots;
- separate intrusive red-black trees of blocked senders and receivers.

The active slots start at `head` and span `len` entries modulo `N`. `0 <= len <= N`
and `head < N` always hold. Only occupied slots are read. Sending writes an
unoccupied tail slot before increasing `len`. Receiving copies the initialized
head slot before advancing `head` and decreasing `len`. Old bits in a vacated slot
are never read until another send initializes that slot.

Index wrap uses comparisons and subtraction rather than integer division.
Non-power-of-two capacities work, and index arithmetic avoids an overflowing
`head + len`. There is no payload allocation, destructor, user-supplied comparison,
or payload callback inside the critical section. Copying a larger `T` still takes
more interrupt-masked time.

The trees reuse each thread's existing `SyncEntity` links. The global blocked
thread index uses that same thread's separate `WaitEntity` links. No thread
structure or context-switch assembly layout changes, and no per-wait heap or
stack node is retained by the queue. A blocked sender keeps its message in its
own suspended call frame; the queue holds only a thread's intrusive waiter node,
never a pointer to its payload or the receiver's output.

All queue access uses the platform critical section, including host builds. The
host platform now delegates its normal library path to the existing reentrant
test-support mutex; previously that path did not lock. Its nesting depth is
restored by a guard even during panic unwinding. This makes the queue's `Send`/
`Sync` implementations usable for native concurrency tests as well as single-core
Cortex-M code. Mutable queue-storage borrows end before wakeup trace callbacks.

## Blocking, wakeup, and ordering

The blocking path performs the availability check and waiter registration in one
critical section. A producer cannot publish between an empty check and receiver
registration, and a consumer cannot free space between a full check and sender
registration. This prevents a missed wakeup at the check-to-sleep boundary.

When unavailable, the queue uses the existing synchronization block helper to
account elapsed scheduler time, move the thread out of its runnable index, and
register an indefinite wait. `insert_wait_thread()` selects `Suspended` because
`wake_at == u64::MAX`; the queue does not force `Waiting`. The thread is also
inserted into the relevant per-queue tree and PendSV is requested. After the
critical section restores interrupt state, barriers allow the pending context
switch to take effect before the operation retries.

Each successful send removes at most one receiver from the queue's receiver
tree. Each successful receive removes at most one sender. Existing wake helpers
remove the global suspended-list entry, set `Ready`, and restore CFS run-queue or
RT timer membership. The scheduler subsequently transitions `Ready -> Running`.
Wakeups use the existing trace counters and callbacks.

Waiters are ordered by their scheduler deadline captured when they block:
the CFS scheduler timer's deadline for a CFS thread, or the individual RT timer's
deadline for an RT thread. Equal deadlines use the existing `SyncEntity` address
tie-breaker. This is urgency ordering, not registration FIFO. No additional
deadline donation is performed because a message queue has no single owner.

Message FIFO order is independent of waiter ordering. A message becomes queued
when its slot is committed, and a receive removes the oldest committed message.
A waking thread owns no reservation. Another thread or interrupt may consume the
message or slot before the waiter resumes; the waiter checks again and suspends
again if necessary. Therefore earliest-waiter wakeup does not guarantee which
thread eventually completes first, starvation freedom, or bounded blocking time.
This tradeoff avoids stack payload pointers, token reservations, and extra
per-thread state while keeping nonblocking operations available to any caller.

Wait diagnostics distinguish `SyncType::MessageQueueSend` (`mqtx`) and
`SyncType::MessageQueueReceive` (`mqrx`). The library timer dump and LPC55S69 shell
display `message-queue-send` or `message-queue-receive`.

## Performance model

Let `S` be the payload size, `W` the relevant waiter count, `K` the timer count,
and `R` the queued runnable CFS thread count. Timer insertion/removal costs `F`,
O(log(K + 1)) in optimized builds. Active and inactive timers use separate trees,
so removing the first active timer does not scan inactive timers. Debug builds
also scan for duplicate membership during insertion. Costs below exclude
user-installed trace/print callbacks and the subsequent PendSV context switch.

| Path | Work while interrupts are masked |
| --- | --- |
| Successful send/receive, no waiters | O(S) payload copy and O(1) ring metadata; no tree scan |
| Full `try_send` / empty `try_receive` | O(1); no payload copy or state change |
| Register blocked operation | O(log(W + 1) + F) waiter/timer work and O(1) suspended-list insertion |
| Wake RT peer | O(S + log(W + 1) + F), including timer reinsertion |
| Wake CFS peer | O(S + log(W + 1) + F + R log(R + 1)); the existing CFS enqueue path rebuilds its run tree |

Availability checks and ring storage do not grow with queue capacity, although
larger buffers can change cache behavior on a host. Blocking calls add repeated
attempts and scheduler latency; there is no finite worst-case duration under
arbitrary contention. Even `try_` calls include wakeup costs when a peer is
blocked, so they are non-suspending rather than a promise of constant interrupt
latency. In particular, CFS tree rebuilding comes from the existing scheduler;
include it and timer maintenance in device latency measurements.

Storage is `N * size_of::<T>()`, six machine words of current metadata (head,
length, and two tree roots/counts), plus alignment padding. This is about 24 bytes
of metadata on the 32-bit target and 48 on the 64-bit host; Rust struct layout is
not a stable ABI. For `MessageQueue<u32, 16>` the current target size is 88 bytes
on Cortex-M and the measured host size is 112 bytes. No per-thread memory is
added. A blocked send also retains its ordinary `T` argument on the thread stack.

### Native microbenchmark

Run from the workspace root:

```sh
cargo bench -p rtsched --bench message_queue --features host --target x86_64-unknown-linux-gnu
```

The benchmark warms up 1,000 pairs and records nine samples of 100,000 successful
`try_send`/`try_receive` pairs, with no waiters. `black_box` retains the payload
work. It reports elapsed wall time per pair and queue storage; the extra
full/empty probes each use one sample. An illustrative run on 2026-10-27 used
rustc 1.93.1, the workspace optimized bench profile, and an AMD Ryzen 9 7900X:

| Payload bytes | Capacity | Host queue bytes | Median ns/pair | Min–max ns/pair |
| --- | --- | --- | --- | --- |
| 4 | 1 | 56 | 6.1 | 6.0–6.3 |
| 4 | 16 | 112 | 6.8 | 6.6–9.4 |
| 4 | 64 | 304 | 7.0 | 6.9–7.1 |
| 32 | 16 | 560 | 20.0 | 19.9–20.6 |
| 128 | 16 | 2,096 | 23.6 | 23.5–23.7 |

Empty and full probes were each 2.9 ns/call in that run. These values include
the uncontended host reentrant mutex path in a single OS thread. They are useful
for regression comparison on the same host/toolchain, not Cortex-M cycle counts,
interrupt latency, contended throughput, or deadline guarantees. CPU frequency,
host load, compiler specialization, and payload layout affect results.

### Cortex-M example and measurement

`examples/message_queue.rs` runs a CFS producer and a slower periodic RT
consumer through a four-slot queue. Sequence numbers and error counters make
FIFO delivery and overflow-free backpressure observable. Startup lets the RT
consumer wait for the first message; after sustained production, the bounded
queue also causes the producer to wait for space.

Build with the board's existing linker memory map:

```sh
cargo rustc -p rtsched --release --example message_queue \
  --features embedded-examples --target thumbv8m.main-none-eabihf \
  -- -L boards/lpc55s69
```

Before starting threads, the example attempts to enable DWT and measures 256
no-waiter `u32` send/receive pairs. Inspect these debugger symbols:

- `PROFILE_DWT_AVAILABLE`: `1` if cycle counting initialized successfully;
- `PROFILE_PAIR_MIN_CYCLES`, `PROFILE_PAIR_MAX_CYCLES`: ring-path pair costs,
  including counter-read overhead, with interrupts masked;
- `PROFILE_READ_MIN_CYCLES`: two-read baseline for interpreting those costs;
- `PRODUCED`, `CONSUMED`: progress; a small difference is expected from buffering
  and from preemption between enqueue and the producer's counter update;
- `SEQUENCE_ERRORS`, `MESSAGE_ERRORS`: should stay zero;
- `LAST_DELIVERY_CYCLES`: time from constructing a sample to consuming it,
  including producer backpressure, buffering, scheduling, and preemption.

The cycle intervals use wrapping `u32` subtraction and must remain shorter than
one counter wrap. Cycle results are meaningful only when DWT is available. The
example inherits the common 12 MHz clock assumption; set it to match real board
configuration. For wakeup latency, additionally use the existing `TraceEvent`
wakeup/context-switch hooks and short DWT timestamps, recording CFS and RT peers
separately. Avoid logging inside measured callbacks. Device measurements have
not been collected as part of this implementation; the example has been
cross-compiled, not flashed or executed on a board.

## Validation

`tests/test_message.rs` covers capacity one, full/empty errors, zero capacity, zero-size
payloads, repeated wraparound against a FIFO model, and multi-producer/consumer
native delivery without duplicates or omissions. Scheduler tests cover CFS and RT
blocking in both directions, reblocking after a competitor uses the resource,
deadline/address wake ordering, independent queues, event tags, and invalid
blocking contexts. A compile-fail doctest rejects sharing non-`Send` payloads.

Scheduler tests inject deterministic work at the point where the embedded caller
would context-switch; they verify the actual block/retry/wake code without
pretending the host can execute PendSV. Existing scheduler, timer, semaphore,
mutex, and wait-queue suites remain part of regression validation.

Validation completed for this implementation:

- Default native suite: 140 integration tests and two doctests passed.
- Native library/tests with all features: 143 tests passed. Select `--lib --tests`
  with `--all-features` on the host; the `embedded-examples` feature enables
  firmware programs whose panic handlers are incompatible with native `std`.
- All embedded examples passed `cargo check` for `thumbv8m.main-none-eabihf`.
- The message queue example linked in release mode with the LPC55S69 memory map; both board
  binaries built with `--release -p lpc55s69 --features board-rt`.
- Rustdoc built with warnings denied. Formatting and diff whitespace checks passed.
- Native library/tests/bench Clippy passed with warnings denied and only
  `clippy::question_mark` allowed for an existing `let ... else` in `waitq.rs`.
  No new source lint allowances were added.
