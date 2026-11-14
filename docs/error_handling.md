# Error handling policy

[Documentation index](../README.md)

`rtsched` uses three failure styles:

- Return `Result` or `Option` for runtime state that board code can handle, such as
  wait-queue transitions, missing current RT runtime, and missing next timer reload.
- Panic for public setup mistakes that would make scheduler state invalid, such as
  zero CFS priority, a null RT timer, null thread storage, null stack storage,
  too-small stacks, or unaligned stack tops. The builders' `spawn` methods
  validate these setup checks before writing thread storage; the former
  `try_spawn` methods are no longer part of the API.
  The unit-returning `yieldyi` and `msleepyi` APIs also panic on invalid calls:
  no selected scheduler thread, a non-idle thread that is not `Running`, or
  context switching unavailable. These checks run before changing timers or
  queues and apply in release builds too. Calls by the idle thread do nothing.
- Use `debug_assert!` for internal unsafe invariants that should already be guaranteed by
  safe wrappers or scheduler ownership rules, such as intrusive-tree duplicate insertion
  checks and raw pointer downcasts.

`BinarySemaphore::take`, `CountingSemaphore::take`, and `Mutex::lock` reject a
required wait with `InvalidContext` unless the selected thread is `Running`,
non-idle, and context switching is available. On Cortex-M, the caller must be in
thread mode with PRIMASK, BASEPRI, and FAULTMASK clear. The checks read the caller's
interrupt masks before entering the operation's own critical section. Do not call
these operations from an interrupt handler or an outer critical section when
they could need to wait; use `try_take` or `try_lock` instead.

Rejection leaves tokens, ownership, waiter queues, and owner deadlines unchanged.
An available token can still be taken immediately, including before the scheduler
selects a thread. A mutex always requires a selected thread to identify its owner;
an unlocked mutex can then be acquired immediately even when blocking is
unavailable. Missing threads return `NoCurrentThread`, and recursive mutex locking
still returns `WouldDeadlock`. Native host builds reject required waits because
they cannot suspend scheduler threads through PendSV. Message queues use the
[same blocking-context policy](message_queue.md).

`yieldyi` and `msleepyi` require the same unmasked thread-mode context for
non-idle threads. Native host builds reject them because they cannot suspend
scheduler threads. Unlike the synchronization APIs, they panic on invalid
contexts because their existing signatures do not return a `Result`.
