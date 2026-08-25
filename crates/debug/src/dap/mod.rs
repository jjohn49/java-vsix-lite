//! The DAP session: a single-task event loop selecting over client
//! requests, JDWP events, debuggee output, and debuggee exit — all funneled
//! through one `mpsc` channel so state needs no locking.

mod protocol;
mod session;

pub use session::run_stdio_adapter;
