# Threads

[Documentation index](../README.md) · Source: [`src/thread.rs`](../src/thread.rs)

`CfsThread` and `RtThread` represent application threads with dedicated stacks.
Their builders return `ThreadHandle` values, and `ThreadId` identifies spawned
threads without passing raw thread-context pointers through application code.
The board owns the concrete thread and stack storage for the scheduler's lifetime.
`IdleThread` provides the [scheduler fallback](sched.md#cpu_idle-thread-for-power-saving).

## Thread states

Each thread has an explicit `ThreadState`:

- `ReadyActive`: eligible to run when selected by the scheduler.
- `ReadyInactive`: an RT job has completed and awaits its next periodic release.
- `Running`: currently executing on the CPU.
- `Waiting`: blocked with a finite timeout, optionally waiting for an event.
- `Suspended`: blocked indefinitely until an external event or resource is ready.

Scheduler code uses `ThreadCtx::set_state(ThreadState)` for runtime state changes.
Normal transitions are `ReadyActive -> Running`, `Running -> ReadyActive`,
`Running/ReadyActive -> Waiting/Suspended`, and `Waiting/Suspended -> ReadyActive`.
An RT job completes with `Running -> ReadyInactive` on `yieldyi()`, then its
next periodic release transitions `ReadyInactive -> ReadyActive`. Blocking
within the current job can transition through `ReadyInactive` to
`Waiting/Suspended` without resetting the accumulated runtime.
Blocked threads must return to `ReadyActive` before they can run again.
Use `ThreadState::is_blocked()` to recognize both kinds of blocked thread.
`ReadyInactive` waits for a periodic release and is not an event-blocked state;
it cannot transition directly to `Running`.

For RT threads, state and timer membership agree:

| Thread state | Queue membership |
| --- | --- |
| `ReadyActive` | `KTimerQueue::active_tree` |
| `Running` | `KTimerQueue::active_tree` |
| `ReadyInactive` | `KTimerQueue::inactive_tree` |
| `Waiting` / `Suspended` | Wait queue; RT timer is detached |

Runnable CFS threads use `ReadyActive` in the CFS run queue. They share the
global CFS timer rather than having individual RT timers. The idle thread also
uses `ReadyActive` when available as the scheduler fallback, without queue
membership.

See [the wait queue guide](waitq.md) for finite waits and indefinite suspension.
