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
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub enum SocketEntry {
    TcpListener(RegisteredTcpListener),
    TcpStream(RegisteredTcpStream),
    UdpSocket(RegisteredUdpSocket),
}

pub struct RegisteredTcpListener {
    listener: TcpListener,
    cancel: Arc<AtomicBool>,
}

pub struct RegisteredTcpStream {
    stream: TcpStream,
    cancel: Arc<AtomicBool>,
}

pub struct RegisteredUdpSocket {
    socket: UdpSocket,
    cancel: Arc<AtomicBool>,
}

pub struct TcpListenerWait {
    pub listener: TcpListener,
    pub cancel: Arc<AtomicBool>,
}

pub struct TcpStreamWait {
    pub stream: TcpStream,
    pub cancel: Arc<AtomicBool>,
}

pub struct UdpSocketWait {
    pub socket: UdpSocket,
    pub cancel: Arc<AtomicBool>,
}

impl SocketEntry {
    fn cancel(&self) {
        match self {
            SocketEntry::TcpListener(entry) => entry.cancel.store(true, Ordering::SeqCst),
            SocketEntry::TcpStream(entry) => entry.cancel.store(true, Ordering::SeqCst),
            SocketEntry::UdpSocket(entry) => entry.cancel.store(true, Ordering::SeqCst),
        }
    }
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
        match self.sockets.remove(&id) {
            Some(entry) => {
                entry.cancel();
                true
            }
            None => false,
        }
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
    Ok(SOCKET_TABLE.with(|t| {
        t.borrow_mut()
            .insert(SocketEntry::TcpListener(RegisteredTcpListener {
                listener,
                cancel: Arc::new(AtomicBool::new(false)),
            }))
    }))
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
            SocketEntry::TcpListener(entry) => entry
                .listener
                .try_clone()
                .map_err(|e| format!("clone listener failed: {}", e)),
            _ => Err("handle is not a TCP listener".into()),
        }
    })?;
    let (stream, addr) = listener
        .accept()
        .map_err(|e| format!("accept failed: {}", e))?;
    let address = addr.to_string();
    let conn_id = insert_tcp_stream(stream);
    Ok((conn_id, address))
}

pub fn tcp_connect(host: &str, port: u16) -> Result<u32, String> {
    let addr = format!("{}:{}", host, port);
    let stream =
        TcpStream::connect(&addr).map_err(|e| format!("failed to connect to {}: {}", addr, e))?;
    Ok(insert_tcp_stream(stream))
}

pub fn insert_tcp_stream(stream: TcpStream) -> u32 {
    SOCKET_TABLE.with(|t| {
        t.borrow_mut()
            .insert(SocketEntry::TcpStream(RegisteredTcpStream {
                stream,
                cancel: Arc::new(AtomicBool::new(false)),
            }))
    })
}

pub fn tcp_read(handle: u32) -> Result<String, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP connection handle".to_string())?;
        match entry {
            SocketEntry::TcpStream(entry) => {
                let mut buf = [0u8; 8192];
                let n = entry
                    .stream
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
            SocketEntry::TcpStream(entry) => read_line_from_stream(&mut entry.stream),
            _ => Err("handle is not a TCP connection".into()),
        }
    })
}

fn read_line_from_stream(stream: &mut TcpStream) -> Result<String, String> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let n = stream
            .read(&mut byte)
            .map_err(|e| format!("readLine failed: {}", e))?;
        if n == 0 {
            break;
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

pub fn tcp_write(handle: u32, data: &[u8]) -> Result<usize, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP connection handle".to_string())?;
        match entry {
            SocketEntry::TcpStream(entry) => {
                entry
                    .stream
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
            SocketEntry::TcpStream(entry) => entry
                .stream
                .peer_addr()
                .map(|a| a.to_string())
                .map_err(|e| format!("address failed: {}", e)),
            SocketEntry::TcpListener(entry) => entry
                .listener
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
            SocketEntry::TcpListener(entry) => {
                entry.listener.local_addr().map_err(|e| e.to_string())
            }
            SocketEntry::TcpStream(entry) => entry.stream.local_addr().map_err(|e| e.to_string()),
            SocketEntry::UdpSocket(entry) => entry.socket.local_addr().map_err(|e| e.to_string()),
        }
    })
}

pub fn clone_tcp_listener_for_wait(handle: u32) -> Result<TcpListenerWait, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP listener handle".to_string())?;
        match entry {
            SocketEntry::TcpListener(entry) => Ok(TcpListenerWait {
                listener: entry
                    .listener
                    .try_clone()
                    .map_err(|e| format!("clone listener failed: {}", e))?,
                cancel: Arc::clone(&entry.cancel),
            }),
            _ => Err("handle is not a TCP listener".into()),
        }
    })
}

pub fn clone_tcp_stream_for_wait(handle: u32) -> Result<TcpStreamWait, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid TCP connection handle".to_string())?;
        match entry {
            SocketEntry::TcpStream(entry) => Ok(TcpStreamWait {
                stream: entry
                    .stream
                    .try_clone()
                    .map_err(|e| format!("clone stream failed: {}", e))?,
                cancel: Arc::clone(&entry.cancel),
            }),
            _ => Err("handle is not a TCP connection".into()),
        }
    })
}

// ── UDP ──────────────────────────────────────────────────────────────

pub fn udp_bind(port: u16) -> Result<u32, String> {
    let addr = format!("0.0.0.0:{}", port);
    let socket =
        UdpSocket::bind(&addr).map_err(|e| format!("failed to bind UDP on {}: {}", addr, e))?;
    Ok(SOCKET_TABLE.with(|t| {
        t.borrow_mut()
            .insert(SocketEntry::UdpSocket(RegisteredUdpSocket {
                socket,
                cancel: Arc::new(AtomicBool::new(false)),
            }))
    }))
}

pub fn udp_send(handle: u32, host: &str, port: u16, data: &[u8]) -> Result<usize, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid UDP socket handle".to_string())?;
        match entry {
            SocketEntry::UdpSocket(entry) => {
                let addr = format!("{}:{}", host, port);
                entry
                    .socket
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
            SocketEntry::UdpSocket(entry) => {
                let mut buf = vec![0u8; 65_535];
                let (n, addr) = entry
                    .socket
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
            SocketEntry::UdpSocket(entry) => entry
                .socket
                .set_broadcast(enabled)
                .map_err(|e| format!("set_broadcast failed: {}", e)),
            _ => Err("handle is not a UDP socket".into()),
        }
    })
}

pub fn clone_udp_socket_for_wait(handle: u32) -> Result<UdpSocketWait, String> {
    SOCKET_TABLE.with(|t| {
        let mut table = t.borrow_mut();
        let entry = table
            .get_mut(handle)
            .ok_or_else(|| "invalid UDP socket handle".to_string())?;
        match entry {
            SocketEntry::UdpSocket(entry) => Ok(UdpSocketWait {
                socket: entry
                    .socket
                    .try_clone()
                    .map_err(|e| format!("clone UDP socket failed: {}", e))?,
                cancel: Arc::clone(&entry.cancel),
            }),
            _ => Err("handle is not a UDP socket".into()),
        }
    })
}
