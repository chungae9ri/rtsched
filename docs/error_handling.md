# Error handling policy

[Documentation index](../README.md)

`rtsched` uses three failure styles:

- Return `Result` or `Option` for runtime state that board code can handle, such as
  wait-queue transitions, missing current RT runtime, and missing next timer reload.
- Panic for public setup mistakes that would make scheduler state invalid, such as
  zero CFS priority, a null RT timer, null thread storage, null stack storage,
  too-small stacks, or unaligned stack tops. The thread builders also provide
  `try_spawn` variants that return `ThreadSpawnError` for these setup checks.
- Use `debug_assert!` for internal unsafe invariants that should already be guaranteed by
  safe wrappers or scheduler ownership rules, such as intrusive-tree duplicate insertion
  checks and raw pointer downcasts.
