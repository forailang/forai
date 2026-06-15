//! The blocking boundary: a worker pool that runs blocking external work off
//! the main (scheduler) thread.
//!
//! forai runs on a single thread because wasmtime allows exactly one in-flight
//! call per `Store`. Blocking work (outbound HTTP, sleep, FFI/SQLite) does not
//! need the `Store`, so it is handed to a worker thread here. The main thread
//! copies arguments out of guest memory into owned `Send` data *before* a job
//! is submitted, the worker runs the blocking call with only that owned data,
//! and the main thread copies the owned result back into guest memory *after*
//! the job completes. A worker never touches the `Store` or guest memory.
//!
//! This unit (U1) is the substrate only: submit jobs, collect completions, and
//! wake the main thread when one is ready. Connecting completions to the guest
//! scheduler's `__fai_resume_task` is U2; the unified driver loop that waits on
//! this alongside socket readiness is U3.
//!
//! `#![allow(dead_code)]` is temporary — these items become live once U2+ wire
//! the boundary to the scheduler and the I/O surfaces.
#![allow(dead_code)]

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The owned, `Send` result a job produces, or an error string if the blocking
/// call failed or panicked. The result is type-erased so every I/O surface can
/// return its own owned shape (an HTTP tuple, an FFI value, `()` for sleep);
/// the main-thread consumer downcasts it when marshalling back (U2).
pub(crate) type JobResult = Result<Box<dyn Any + Send>, String>;

/// A unit of blocking work plus the guest task it belongs to.
type Job = (i32, Box<dyn FnOnce() -> Box<dyn Any + Send> + Send>);

/// A finished job, ready for the main thread to marshal back and resume.
pub(crate) struct Completion {
    pub task_id: i32,
    pub result: JobResult,
}

/// Worker-facing shared state: the pending job queue and a shutdown flag,
/// guarded together so a worker waits on one condvar for either event.
struct JobQueue {
    jobs: VecDeque<Job>,
    shutdown: bool,
}

struct Shared {
    queue: Mutex<JobQueue>,
    /// Signaled when a job is enqueued or shutdown is requested.
    available: Condvar,
}

/// Main-thread-facing completion side: finished jobs and a condvar the driver
/// loop blocks on when nothing else is runnable.
struct Completions {
    queue: Mutex<VecDeque<Completion>>,
    ready: Condvar,
}

/// A fixed pool of worker threads. The pool size caps how many blocking jobs
/// run at once (R7 backpressure); excess jobs queue until a worker frees up.
pub(crate) struct Boundary {
    shared: Arc<Shared>,
    completions: Arc<Completions>,
    /// Jobs submitted but not yet drained as completions. Lets the driver loop
    /// know whether it is waiting on outstanding work.
    inflight: Arc<AtomicUsize>,
    workers: Vec<JoinHandle<()>>,
}

impl Boundary {
    pub(crate) fn new(pool_size: usize) -> Self {
        let pool_size = pool_size.max(1);
        let shared = Arc::new(Shared {
            queue: Mutex::new(JobQueue {
                jobs: VecDeque::new(),
                shutdown: false,
            }),
            available: Condvar::new(),
        });
        let completions = Arc::new(Completions {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
        });

        let mut workers = Vec::with_capacity(pool_size);
        for i in 0..pool_size {
            let shared = Arc::clone(&shared);
            let completions = Arc::clone(&completions);
            let handle = thread::Builder::new()
                .name(format!("fai-boundary-{i}"))
                .spawn(move || worker_loop(shared, completions))
                .expect("spawn boundary worker");
            workers.push(handle);
        }

        Boundary {
            shared,
            completions,
            inflight: Arc::new(AtomicUsize::new(0)),
            workers,
        }
    }

    /// Submit blocking `work` for `task_id`. `work` runs on a worker thread and
    /// must touch only owned `Send` data — never the `Store` or guest memory.
    pub(crate) fn submit<F>(&self, task_id: i32, work: F)
    where
        F: FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.jobs.push_back((task_id, Box::new(work)));
        }
        self.shared.available.notify_one();
    }

    /// Drain every completion ready so far without blocking. Each drained
    /// completion decrements the in-flight count.
    pub(crate) fn drain_completions(&self) -> Vec<Completion> {
        let mut q = self.completions.queue.lock().unwrap();
        let drained: Vec<Completion> = q.drain(..).collect();
        if !drained.is_empty() {
            self.inflight.fetch_sub(drained.len(), Ordering::SeqCst);
        }
        drained
    }

    /// Block until at least one completion is ready or `timeout` elapses.
    /// Returns true if a completion is ready to drain.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let q = self.completions.queue.lock().unwrap();
        if !q.is_empty() {
            return true;
        }
        let (q, _timed_out) = self
            .completions
            .ready
            .wait_timeout(q, timeout)
            .unwrap();
        !q.is_empty()
    }

    /// Jobs submitted but not yet drained. Zero means no outstanding work.
    pub(crate) fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }
}

impl Drop for Boundary {
    fn drop(&mut self) {
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.shutdown = true;
        }
        self.shared.available.notify_all();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(shared: Arc<Shared>, completions: Arc<Completions>) {
    loop {
        let (task_id, work) = {
            let mut q = shared.queue.lock().unwrap();
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    break job;
                }
                if q.shutdown {
                    return;
                }
                q = shared.available.wait(q).unwrap();
            }
        };

        // A blocking call must not take the whole pool down if it panics;
        // convert a panic into a failed completion for this task.
        let result: JobResult = match catch_unwind(AssertUnwindSafe(work)) {
            Ok(value) => Ok(value),
            Err(_) => Err("boundary job panicked".to_string()),
        };

        {
            let mut q = completions.queue.lock().unwrap();
            q.push_back(Completion { task_id, result });
        }
        completions.ready.notify_one();
    }
}

/// Default pool size: scale with cores, but keep it small — these threads exist
/// to absorb blocking I/O, not to do CPU work in parallel.
fn default_pool_size() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
}

thread_local! {
    static BOUNDARY: RefCell<Option<Boundary>> = const { RefCell::new(None) };
}

/// Access the main thread's boundary, lazily creating its worker pool on first
/// use. Host imports call this from inside a `Caller` (always the main thread).
pub(crate) fn with_boundary<R>(f: impl FnOnce(&Boundary) -> R) -> R {
    BOUNDARY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(Boundary::new(default_pool_size()));
        }
        f(slot.as_ref().unwrap())
    })
}

/// Like `with_boundary` but never creates the pool — returns None if no boundary
/// has been used yet, so a driver loop can pump cheaply on every iteration
/// without spawning worker threads for programs that never offload.
fn try_with_boundary<R>(f: impl FnOnce(&Boundary) -> R) -> Option<R> {
    BOUNDARY.with(|slot| slot.borrow().as_ref().map(f))
}

thread_local! {
    /// Finished job results awaiting the guest's matching `*_result` import,
    /// keyed by the task that parked on the job. Populated by `pump_ready`,
    /// drained by `take_ready` (e.g. `remote_result`).
    static READY: RefCell<HashMap<i32, JobResult>> = RefCell::new(HashMap::new());
}

/// Drain every finished job from the worker pool into the per-task ready map and
/// return the task ids now ready to resume. The driver loop calls this each
/// iteration, then `__fai_resume_task`s each returned id. Cheap no-op (empty
/// vec) when no boundary exists or nothing finished.
pub(crate) fn pump_ready() -> Vec<i32> {
    let Some(completions) = try_with_boundary(|b| b.drain_completions()) else {
        return Vec::new();
    };
    if completions.is_empty() {
        return Vec::new();
    }
    let mut ids = Vec::with_capacity(completions.len());
    READY.with(|r| {
        let mut map = r.borrow_mut();
        for c in completions {
            ids.push(c.task_id);
            map.insert(c.task_id, c.result);
        }
    });
    ids
}

/// True if any job is still running or waiting to be drained — lets a driver
/// loop decide whether it must keep polling.
pub(crate) fn has_inflight() -> bool {
    try_with_boundary(|b| b.inflight() > 0).unwrap_or(false)
}

/// Take the finished result for `task_id` (surfaced by `pump_ready`). The guest
/// calls its `*_result` import after being resumed; that import calls this.
pub(crate) fn take_ready(task_id: i32) -> Option<JobResult> {
    READY.with(|r| r.borrow_mut().remove(&task_id))
}

/// Tear down the main thread's boundary (joins workers). Used by test teardown
/// and finite-run cleanup.
pub(crate) fn shutdown_boundary() {
    BOUNDARY.with(|slot| {
        slot.borrow_mut().take();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn drain_until<F: Fn(&[Completion]) -> bool>(
        b: &Boundary,
        want: usize,
        check: F,
    ) -> Vec<Completion> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < want && Instant::now() < deadline {
            b.wait(Duration::from_millis(200));
            got.extend(b.drain_completions());
        }
        assert!(check(&got));
        got
    }

    #[test]
    fn runs_jobs_and_returns_owned_results() {
        let b = Boundary::new(2);
        for n in 0..5i64 {
            b.submit(n as i32, move || Box::new(n * 10) as Box<dyn Any + Send>);
        }
        let got = drain_until(&b, 5, |c| c.len() == 5);
        let mut by_task: Vec<(i32, i64)> = got
            .into_iter()
            .map(|c| (c.task_id, *c.result.unwrap().downcast::<i64>().unwrap()))
            .collect();
        by_task.sort();
        assert_eq!(by_task, vec![(0, 0), (1, 10), (2, 20), (3, 30), (4, 40)]);
        assert_eq!(b.inflight(), 0);
    }

    #[test]
    fn never_exceeds_pool_size_concurrently() {
        const POOL: usize = 3;
        let b = Boundary::new(POOL);
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        for n in 0..12i32 {
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            b.submit(n, move || {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(20));
                live.fetch_sub(1, Ordering::SeqCst);
                Box::new(()) as Box<dyn Any + Send>
            });
        }
        drain_until(&b, 12, |c| c.len() == 12);
        assert!(peak.load(Ordering::SeqCst) <= POOL, "peak exceeded pool size");
    }

    #[test]
    fn wait_unblocks_on_completion() {
        let b = Boundary::new(1);
        // Nothing submitted yet: wait should time out (no completion).
        assert!(!b.wait(Duration::from_millis(50)));
        b.submit(7, || {
            thread::sleep(Duration::from_millis(30));
            Box::new(99i64) as Box<dyn Any + Send>
        });
        // Should be woken within the timeout once the job finishes.
        assert!(b.wait(Duration::from_secs(2)));
        let got = b.drain_completions();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].task_id, 7);
    }

    #[test]
    fn panicking_job_becomes_failed_completion() {
        let b = Boundary::new(1);
        b.submit(3, || panic!("boom"));
        // Pool survives and a later job still runs.
        b.submit(4, || Box::new(1i64) as Box<dyn Any + Send>);
        let got = drain_until(&b, 2, |c| c.len() == 2);
        let failed = got.iter().find(|c| c.task_id == 3).unwrap();
        assert!(failed.result.is_err());
        let ok = got.iter().find(|c| c.task_id == 4).unwrap();
        assert!(ok.result.is_ok());
    }
}
