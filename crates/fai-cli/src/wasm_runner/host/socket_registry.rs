//! Thread-local socket handle registry used by TCP and UDP host imports.
//!
//! Handles are `u32` so they round-trip through FAI's Int type without
//! truncation. The registry lives behind a `thread_local!` because the
//! wasm runner is single-threaded per run and this avoids wiring a state
//! struct into every host closure signature.
//!
//! Mirrors the layout and semantics of `fai-runtime::natives::net_impl` —
//! kept separate so Plan 93 can delete the VM's copy without breaking the
//! wasm side.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};

pub enum SocketEntry {
    TcpListener(TcpListener),
    TcpStream(BufReader<TcpStream>),
    UdpSocket(UdpSocket),
}

pub struct SocketTable {
    next_id: u32,
    sockets: HashMap<u32, SocketEntry>,
}

impl SocketTable {
    fn new() -> Self {
        Self {
            next_id: 1,
            sockets: HashMap::new(),
        }
    }
    pub fn insert(&mut self, entry: SocketEntry) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.sockets.insert(id, entry);
        id
    }
    pub fn get_mut(&mut self, id: u32) -> Option<&mut SocketEntry> {
        self.sockets.get_mut(&id)
    }
    pub fn remove(&mut self, id: u32) -> bool {
        self.sockets.remove(&id).is_some()
    }
}

thread_local! {
    pub(super) static SOCKET_TABLE: RefCell<SocketTable> = RefCell::new(SocketTable::new());
}

// ── TCP ──────────────────────────────────────────────────────────────

pub fn tcp_listen(port: u16) -> Result<u32, String> {
    let addr = format!("0.0.0.0:{}", port);
    let listener =
        TcpListener::bind(&addr).map_err(|e| format!("failed to bind TCP on {}: {}", addr, e))?;
    Ok(SOCKET_TABLE.with(|t| t.borrow_mut().insert(SocketEntry::TcpListener(listener))))
}

pub fn tcp_accept(handle: u32) -> Result<(u32, String), String> {
    // Grab the listener, release the borrow during the blocking accept,
    // then re-borrow to insert the new stream. Keeping one borrow across
    // accept would deadlock any other host import the peer's handler
    // tries to use (the accept loop pattern in native_tcp_listen hits
    // this on every connection).
    let listener = SOCKET_TABLE.with(|t| -> Result<TcpListener, String> {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP listener handle".to_string())?;
        match entry {
            SocketEntry::TcpListener(l) => l
                .try_clone()
                .map_err(|e| format!("clone listener failed: {}", e)),
            _ => Err("handle is not a TCP listener".into()),
        }
    })?;
    let (stream, addr) = listener
        .accept()
        .map_err(|e| format!("accept failed: {}", e))?;
    let address = addr.to_string();
    let conn_id = SOCKET_TABLE.with(|t| {
        t.borrow_mut()
            .insert(SocketEntry::TcpStream(BufReader::new(stream)))
    });
    Ok((conn_id, address))
}

pub fn tcp_connect(host: &str, port: u16) -> Result<u32, String> {
    let addr = format!("{}:{}", host, port);
    let stream =
        TcpStream::connect(&addr).map_err(|e| format!("failed to connect to {}: {}", addr, e))?;
    Ok(SOCKET_TABLE.with(|t| {
        t.borrow_mut()
            .insert(SocketEntry::TcpStream(BufReader::new(stream)))
    }))
}

pub fn tcp_read(handle: u32) -> Result<String, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP connection handle".to_string())?;
        match entry {
            SocketEntry::TcpStream(reader) => {
                let mut buf = [0u8; 8192];
                let n = reader
                    .read(&mut buf)
                    .map_err(|e| format!("read failed: {}", e))?;
                Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
            }
            _ => Err("handle is not a TCP connection".into()),
        }
    })
}

pub fn tcp_read_line(handle: u32) -> Result<String, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP connection handle".to_string())?;
        match entry {
            SocketEntry::TcpStream(reader) => {
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .map_err(|e| format!("readLine failed: {}", e))?;
                Ok(line)
            }
            _ => Err("handle is not a TCP connection".into()),
        }
    })
}

pub fn tcp_write(handle: u32, data: &[u8]) -> Result<usize, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP connection handle".to_string())?;
        match entry {
            SocketEntry::TcpStream(reader) => {
                let stream = reader.get_mut();
                stream
                    .write_all(data)
                    .map_err(|e| format!("write failed: {}", e))?;
                Ok(data.len())
            }
            _ => Err("handle is not a TCP connection".into()),
        }
    })
}

pub fn tcp_address(handle: u32) -> Result<String, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP socket handle".to_string())?;
        match entry {
            SocketEntry::TcpStream(reader) => reader
                .get_ref()
                .peer_addr()
                .map(|a| a.to_string())
                .map_err(|e| format!("address failed: {}", e)),
            SocketEntry::TcpListener(listener) => listener
                .local_addr()
                .map(|a| a.to_string())
                .map_err(|e| format!("address failed: {}", e)),
            _ => Err("handle is not a TCP socket".into()),
        }
    })
}

pub fn socket_close(handle: u32) -> Result<(), String> {
    SOCKET_TABLE.with(|t| {
        if t.borrow_mut().remove(handle) {
            Ok(())
        } else {
            Err("invalid socket handle".into())
        }
    })
}

#[allow(dead_code)]
pub fn socket_local_addr(handle: u32) -> Result<SocketAddr, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid socket handle".to_string())?;
        match entry {
            SocketEntry::TcpListener(l) => l.local_addr().map_err(|e| e.to_string()),
            SocketEntry::TcpStream(r) => r.get_ref().local_addr().map_err(|e| e.to_string()),
            SocketEntry::UdpSocket(s) => s.local_addr().map_err(|e| e.to_string()),
        }
    })
}

// ── UDP ──────────────────────────────────────────────────────────────

pub fn udp_bind(port: u16) -> Result<u32, String> {
    let addr = format!("0.0.0.0:{}", port);
    let socket =
        UdpSocket::bind(&addr).map_err(|e| format!("failed to bind UDP on {}: {}", addr, e))?;
    Ok(SOCKET_TABLE.with(|t| t.borrow_mut().insert(SocketEntry::UdpSocket(socket))))
}

pub fn udp_send(handle: u32, host: &str, port: u16, data: &[u8]) -> Result<usize, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid UDP socket handle".to_string())?;
        match entry {
            SocketEntry::UdpSocket(socket) => {
                let addr = format!("{}:{}", host, port);
                socket
                    .send_to(data, &addr)
                    .map_err(|e| format!("send failed: {}", e))
            }
            _ => Err("handle is not a UDP socket".into()),
        }
    })
}

pub fn udp_receive(handle: u32) -> Result<(Vec<u8>, String, u16), String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid UDP socket handle".to_string())?;
        match entry {
            SocketEntry::UdpSocket(socket) => {
                let mut buf = vec![0u8; 65_535];
                let (n, addr) = socket
                    .recv_from(&mut buf)
                    .map_err(|e| format!("receive failed: {}", e))?;
                buf.truncate(n);
                Ok((buf, addr.ip().to_string(), addr.port()))
            }
            _ => Err("handle is not a UDP socket".into()),
        }
    })
}

pub fn udp_set_broadcast(handle: u32, enabled: bool) -> Result<(), String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid UDP socket handle".to_string())?;
        match entry {
            SocketEntry::UdpSocket(socket) => socket
                .set_broadcast(enabled)
                .map_err(|e| format!("set_broadcast failed: {}", e)),
            _ => Err("handle is not a UDP socket".into()),
        }
    })
}
