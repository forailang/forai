//! The readiness reactor (plan 103 U4): one side thread owning a `mio::Poll`
//! that watches file descriptors and reports "watch N fired" into the
//! boundary's completion queue via a `CompletionSink`.
//!
//! This is the runtime's missing *readiness* wake source. The driver loop
//! parks on the boundary condvar; before the reactor, only a finished job or
//! a timer deadline could wake it, so anywhere the main thread needed to
//! watch a socket it fell back to sleep+re-poll. A watch turns "fd became
//! readable" into the same wake as a completed job.
//!
//! Design constraints:
//! - The reactor never reads, accepts, or touches guest memory (KTD4). It
//!   only reports readiness; the main thread does the non-blocking I/O.
//! - Watches are **one-shot**: a fired watch is deregistered before it is
//!   reported, so a level-triggered fd can't storm the queue. Re-arm by
//!   watching again.
//! - Unix only (`SourceFd`); the callers keep their sleep+re-poll fallback
//!   on other platforms.

#![cfg(unix)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};

use super::boundary::{CompletionSink, REACTOR_EVENT_JOB_ID};

/// Token reserved for the cross-thread waker; watch ids start above it.
const WAKER_TOKEN: Token = Token(0);

enum Command {
    /// Watch `fd` for readability, reporting `watch_id` once when it fires.
    Watch { watch_id: u64, fd: RawFd },
    /// Drop a watch before it fires (e.g. the socket was closed / the wait
    /// was cancelled). Safe to call after the fire; unknown ids are ignored.
    Unwatch { watch_id: u64 },
}

struct ReactorHandle {
    tx: Sender<Command>,
    waker: Arc<Waker>,
    next_watch_id: std::cell::Cell<u64>,
}

thread_local! {
    /// One reactor per driver thread, mirroring the thread-local boundary its
    /// sink pushes into (tests run driver loops on many threads; production
    /// has one). Lazily started on first watch; the reactor thread is a
    /// daemon — it parks in `poll` when unused.
    static REACTOR: RefCell<Option<ReactorHandle>> = const { RefCell::new(None) };
}

/// Start watching `fd` for readability. Returns the watch id that will be
/// reported through `boundary::take_readiness()` when the fd fires. One-shot:
/// the watch is consumed by firing. The caller owns the fd's lifetime and
/// must `unwatch` if it closes the fd before the fire.
pub(crate) fn watch_readable(fd: RawFd) -> u64 {
    with_reactor(|r| {
        let watch_id = r.next_watch_id.get();
        r.next_watch_id.set(watch_id + 1);
        let _ = r.tx.send(Command::Watch { watch_id, fd });
        let _ = r.waker.wake();
        watch_id
    })
}

/// Cancel a watch. Idempotent; ignores already-fired/unknown ids.
pub(crate) fn unwatch(watch_id: u64) {
    REACTOR.with(|slot| {
        if let Some(r) = slot.borrow().as_ref() {
            let _ = r.tx.send(Command::Unwatch { watch_id });
            let _ = r.waker.wake();
        }
    });
}

fn with_reactor<R>(f: impl FnOnce(&ReactorHandle) -> R) -> R {
    REACTOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(spawn_reactor(super::boundary::completion_sink()));
        }
        f(slot.as_ref().unwrap())
    })
}

fn spawn_reactor(sink: CompletionSink) -> ReactorHandle {
    let poll = Poll::new().expect("create mio poll");
    let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN).expect("create mio waker"));
    let (tx, rx) = channel::<Command>();
    thread::Builder::new()
        .name("fai-reactor".to_string())
        .spawn(move || reactor_loop(poll, rx, sink))
        .expect("spawn reactor thread");
    ReactorHandle {
        tx,
        waker,
        next_watch_id: std::cell::Cell::new(1),
    }
}

fn reactor_loop(mut poll: Poll, rx: Receiver<Command>, sink: CompletionSink) {
    // Live watches: token → (watch_id, fd). Token space starts at 1 so the
    // waker keeps Token(0).
    let mut watches: HashMap<Token, (u64, RawFd)> = HashMap::new();
    let mut by_id: HashMap<u64, Token> = HashMap::new();
    let mut next_token: usize = 1;
    let mut events = Events::with_capacity(64);

    loop {
        if poll.poll(&mut events, None).is_err() {
            // EINTR or a torn-down poll: retry; anything persistent will
            // error again and the loop is a daemon thread either way.
            continue;
        }

        // Drain commands first so an Unwatch racing its own fire wins.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::Watch { watch_id, fd } => {
                    let token = Token(next_token);
                    next_token += 1;
                    if poll
                        .registry()
                        .register(&mut SourceFd(&fd), token, Interest::READABLE)
                        .is_ok()
                    {
                        watches.insert(token, (watch_id, fd));
                        by_id.insert(watch_id, token);
                    } else {
                        // Registration failed (bad fd, duplicate): report the
                        // watch as fired so the caller re-checks the fd state
                        // instead of waiting forever.
                        sink.push(REACTOR_EVENT_JOB_ID, Ok(Box::new(watch_id)));
                    }
                }
                Command::Unwatch { watch_id } => {
                    if let Some(token) = by_id.remove(&watch_id) {
                        if let Some((_, fd)) = watches.remove(&token) {
                            let _ = poll.registry().deregister(&mut SourceFd(&fd));
                        }
                    }
                }
            }
        }

        for event in events.iter() {
            let token = event.token();
            if token == WAKER_TOKEN {
                continue;
            }
            // One-shot: deregister before reporting so a level-triggered fd
            // can't fire again while the report is queued.
            if let Some((watch_id, fd)) = watches.remove(&token) {
                by_id.remove(&watch_id);
                let _ = poll.registry().deregister(&mut SourceFd(&fd));
                sink.push(REACTOR_EVENT_JOB_ID, Ok(Box::new(watch_id)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::boundary;
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    fn wait_for_watch(expected: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            boundary::wait_for_ready(Duration::from_millis(50));
            let ids = {
                boundary::pump_ready();
                boundary::take_readiness()
            };
            if ids.contains(&expected) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    #[test]
    fn listener_watch_fires_on_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let watch = watch_readable(listener.as_raw_fd());

        let started = Instant::now();
        let _client = TcpStream::connect(addr).unwrap();
        assert!(
            wait_for_watch(watch, Duration::from_secs(2)),
            "listener watch did not fire"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "watch fire took {:?}",
            started.elapsed()
        );
        boundary::shutdown_boundary();
    }

    #[test]
    fn stream_watch_fires_on_data_and_is_one_shot() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();

        let watch = watch_readable(server.as_raw_fd());
        client.write_all(b"x").unwrap();
        assert!(
            wait_for_watch(watch, Duration::from_secs(2)),
            "stream watch did not fire"
        );

        // One-shot: more data does not re-fire the consumed watch.
        client.write_all(b"y").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        boundary::pump_ready();
        assert!(
            boundary::take_readiness().is_empty(),
            "one-shot watch fired twice"
        );
        boundary::shutdown_boundary();
    }

    #[test]
    fn unwatch_prevents_fire() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();

        let watch = watch_readable(server.as_raw_fd());
        unwatch(watch);
        client.write_all(b"x").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        boundary::pump_ready();
        assert!(
            !boundary::take_readiness().contains(&watch),
            "unwatched id still fired"
        );
        boundary::shutdown_boundary();
    }
}
