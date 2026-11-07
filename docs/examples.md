# Embedded example programs

[Documentation index](../README.md) · Source: [`examples`](../examples)

The [`examples`](../examples) directory contains small `no_std` Cortex-M programs that
show how board code wires the scheduler together. They are intentionally
board-neutral: each example initializes the scheduler, creates statically
allocated worker threads and stacks, programs SysTick from
`next_ktimer_reload()`, starts `cpu_idle` with `spawn_main_thread()`, and handles
SysTick with `handle_sched_tick()`. The examples use atomic counters and spin
loops as simple observable work instead of board-specific UART or LED drivers.

All examples share [`examples/common/mod.rs`](../examples/common/mod.rs), which provides:

- clock constants for a 12 MHz system clock
- common CFS period and execution-window ticks
- `init_scheduler()` for `update_sys_clk_freq()`, `init_ktimer_queue()`, and
  `init_cfs()`
- `configure_systick()` for Cortex-M SysTick setup
- a `cpu_idle` thread that waits with `wfi`
- a panic handler that parks the CPU in idle

`minimal_cfs.rs` demonstrates the smallest normal CFS setup. It starts one
`cpu_idle` idle thread and creates one runnable `worker` CFS thread. The worker
increments `WORKER_RUNS`, spins briefly, and calls `yieldyi()` so the scheduler
can select the next runnable entity.

`minimal_rt.rs` demonstrates one periodic RT thread. The `control` thread uses
`RtKTimer::new_with_timing()` with a 50 ms period, 20 ms relative deadline, and
5 ms budget. Each job increments `CONTROL_JOBS`, performs a short spin, and
calls `yieldyi()` to finish the current job window. Yielding accounts the
completed job and resets its runtime counter automatically.

`mixed_rt_cfs.rs` demonstrates RT and CFS work sharing the same scheduler. It
creates a background CFS thread plus two RT threads: `fast_rt` with a 40 ms
period, 15 ms deadline, and 4 ms budget, and `slow_rt` with a 100 ms period,
60 ms deadline, and 10 ms budget. The background thread sleeps for 100 ms with
`msleepyi()`, while the RT threads run jobs and yield at completion.

`sleep_wake.rs` demonstrates the wait queue path for CFS threads. It creates two
CFS sleeper threads. `fast_sleeper` increments `FAST_WAKEUPS` and sleeps for
50 ms; `slow_sleeper` increments `SLOW_WAKEUPS` and sleeps for 250 ms. SysTick
advances the timer queue and wakes each thread through the wait queue when its
sleep deadline expires.

`sync_binary_semaphore.rs` demonstrates a one-token handoff with three
contender threads: two `CfsThread`s and one `RtThread`. Each contender takes the
binary semaphore, spins briefly while holding it, and gives it back. The
`RtThread` calls `yieldyi()` at the end of each iteration.

`sync_counting_semaphore.rs` demonstrates bounded token storage and waiter
wakeup. A producer attempts to add three tokens every 120 ms with
`CountingSemaphore::give()`, while fast and slow consumers block in `take()`.
The token and overflow counters show whether tokens are handed directly to
waiters or stored up to the configured maximum.

`sync_mutex.rs` demonstrates mutex ownership transfer between two CFS threads.
Both threads update the same `Mutex<u32>` protected counter. The owner thread
sleeps briefly while holding the guard, causing the contender to block until
the guard drops and wakes the next waiter.

`message_queue.rs` connects a CFS producer and an RT consumer through a
four-slot FIFO. Startup demonstrates send and receive timeouts, then cancels a
blocked receive. Sample traffic uses bounded waits and retries timed-out sends
without skipping sequence numbers. Debugger counters record expected timeouts,
cancellation, sequence errors, progress, delivery latency, and a startup DWT
benchmark of the no-waiter path.
See [the measurement guide](message_queue.md#cortex-m-example-and-measurement)
for the debugger symbols and their interpretation.

Run the following commands from the `rtsched-boards` workspace root.
Compile-check every embedded example with:

```sh
cargo check -p rtsched --examples --features embedded-examples --target thumbv8m.main-none-eabihf
```

To build one example for an LPC55S69 board, pass the board crate directory as a
linker search path so `cortex-m-rt` can find `memory.x`:

```sh
cargo rustc -p rtsched \
  --example minimal_cfs \
  --features embedded-examples \
  --target thumbv8m.main-none-eabihf \
  -- -L boards/lpc55s69
```

Flash the produced ELF with `probe-rs`:

```sh
probe-rs download \
  --chip LPC55S69JBD100 \
  --protocol swd \
  target/thumbv8m.main-none-eabihf/debug/examples/minimal_cfs
```

Replace `minimal_cfs` with `minimal_rt`, `mixed_rt_cfs`, `sleep_wake`,
`sync_binary_semaphore`, `sync_counting_semaphore`, `sync_mutex`, or
`message_queue` to run the other examples.
