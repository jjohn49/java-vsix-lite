//! Pure-Rust DAP↔JDWP debug adapter for java-vsix-lite.
//!
//! Runs as the `dap` subcommand of the `jvl-server` binary: VS Code speaks
//! the Debug Adapter Protocol (JSON with `Content-Length` framing) on this
//! process's stdio, and this crate bridges it to a JDWP connection with the
//! debuggee JVM over loopback TCP. Both protocols are hand-rolled — no
//! JVM-side components, no new native artifacts, no build-script execution.
//!
//! Security posture (see `docs/THREAT_MODEL.md`):
//! - The debuggee is *project code* — launching it is gated on Workspace
//!   Trust in the VS Code extension, never here (same trust-the-caller model
//!   as the `javac` check command).
//! - Everything the debuggee sends over JDWP is **untrusted input**: packet
//!   lengths and decoded strings are capped, malformed data tears the
//!   session down instead of panicking.
//! - All subprocesses (`java`, fallback `javac`) are spawned argv-only —
//!   never through a shell — with bounded output capture, and the JDWP
//!   socket is bound to 127.0.0.1 only.

#![forbid(unsafe_code)]

mod dap;
pub mod jdwp;
mod launch;

pub use dap::run_stdio_adapter;
