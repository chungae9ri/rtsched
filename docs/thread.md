# Threads

[Documentation index](../README.md) · Source: [`src/thread.rs`](../src/thread.rs)

`CfsThread` and `RtThread` represent application threads with dedicated stacks.
Their builders return `ThreadHandle` values, and `ThreadId` identifies spawned
threads without passing raw thread-context pointers through application code.
The board owns the concrete thread and stack storage for the scheduler's lifetime.
`IdleThread` provides the [scheduler fallback](sched.md#cpu_idle-thread-for-power-saving).

## Thread states

Each thread has an explicit `ThreadState`:

- `Ready`: eligible to run when selected by the scheduler.
- `Running`: currently executing on the CPU.
- `Waiting`: blocked with a finite timeout, optionally waiting for an event.
- `Suspended`: blocked indefinitely until an external event or resource is ready.

Scheduler code uses `ThreadCtx::set_state(ThreadState)` for runtime state changes.
Normal transitions are `Ready -> Running`, `Running -> Ready`,
`Running/Ready -> Waiting/Suspended`, and `Waiting/Suspended -> Ready`.
Blocked threads must return to `Ready` before they can run again.
Use `ThreadState::is_blocked()` to recognize both kinds of blocked thread.

See [the wait queue guide](waitq.md) for finite waits and indefinite suspension.
