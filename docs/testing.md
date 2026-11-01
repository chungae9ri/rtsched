# Host tests

[Documentation index](../README.md)

The scheduler data-structure tests run on the native host target across host
operating systems. Find the target triple on the `host:` line of `rustc -vV`
and pass it explicitly. For example, on x86_64 Linux, run from the workspace root:

```sh
cargo test --manifest-path rtsched/Cargo.toml --features host --target x86_64-unknown-linux-gnu
```

The workspace's `cargo test-host` alias enables `host` and uses that same
x86_64 Linux target.
On other hosts, replace `x86_64-unknown-linux-gnu` with the native target triple,
such as `aarch64-apple-darwin` or `x86_64-pc-windows-msvc`.

The `host` feature explicitly selects `HostPlatform` and its test hooks,
independent of the host CPU and OS. It is not enabled automatically by
`cargo test`: integration tests compile the library without `cfg(test)`, so the
feature must select the backend for both library and test builds.

The explicit target matters for board-oriented workspaces because Cargo may
otherwise inherit an embedded default target, which cannot build Rust's host
test harness.

To cover the optional timing diagnostics on the host, use
`--features host,sched-isr-timing`. For `--all-features`, also select
`--lib --tests` to exclude firmware examples that require an embedded target.
For native benchmarks and documentation builds, enable `host` as well. docs.rs
enables it through the package metadata.
