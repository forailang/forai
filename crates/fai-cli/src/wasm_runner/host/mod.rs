//! Host function bindings for the wasmtime linker.

use wasmtime::Linker;

mod array;
mod async_ops;
mod env;
mod events;
mod http_server;
mod io;
mod json;
mod net;
mod socket_registry;
mod sockets;
mod spy;
mod storage;
pub(super) mod util;

pub(crate) use spy::reset_all as reset_spy_state;

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
    util::install(linker)?;
    http_server::install(linker)?;
    storage::install(linker)?;
    env::install(linker)?;
    events::install(linker)?;
    array::install(linker)?;
    sockets::install(linker)?;
    spy::install(linker)?;
    Ok(())
}
