//! Host function bindings for the wasmtime linker.

use wasmtime::Linker;

mod array;
mod async_ops;
pub(crate) mod boundary;
mod crypto;
mod env;
mod events;
mod guest_scheduler;
mod host_ops;
mod http_server;
mod io;
mod json;
mod net;
mod process;
pub(crate) mod reactor;
pub(crate) mod secrets;
mod socket_registry;
mod sockets;
mod spy;
mod storage;
pub(super) mod util;

pub(crate) use async_ops::{park_for_next_event, prune_fired_timers};
pub(crate) use env::parse_dotenv;

/// Dispatch fired reactor watches to their owners (plan 103 U5): socket waits
/// perform their non-blocking I/O and return the guest task ids to resume;
/// ids the sockets don't own are left in `ids` for the caller (the HTTP
/// server's listener/connection watches).
pub(crate) fn dispatch_socket_readiness(ids: &mut Vec<u64>) -> Vec<i32> {
    sockets::handle_ready_watches(ids)
}
pub(crate) use http_server::drain_retained_values as drain_router_retained_values;
pub(crate) use spy::drain_retained_values as drain_spy_retained_values;

#[cfg(test)]
pub(super) use async_ops::clear_timer_requests;

/// Pull any pending trap message left by an assertion-failure import.
/// Called by the CLI test runner right after catching a wasmtime trap.
pub(super) fn take_trap_msg() -> Option<String> {
    io::take_trap_msg()
}

/// Install every `env.*` host function the FAI WASM runtime expects.
pub(super) fn install_all(linker: &mut Linker<()>) -> Result<(), String> {
    io::install(linker)?;
    async_ops::install(linker)?;
    json::install(linker)?;
    net::install(linker)?;
    process::install(linker)?;
    util::install(linker)?;
    host_ops::install(linker)?;
    http_server::install(linker)?;
    storage::install(linker)?;
    env::install(linker)?;
    secrets::install(linker)?;
    events::install(linker)?;
    array::install(linker)?;
    crypto::install(linker)?;
    sockets::install(linker)?;
    spy::install(linker)?;
    Ok(())
}
