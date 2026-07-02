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

/// How a job occupies the boundary (plan 103 U1).
///
/// The bounded pool exists as backpressure for *work* — short, resource-bound
/// jobs (file I/O, FFI calls) where running too many at once helps nothing.
/// A *wait* is the opposite profile: peer- or child-paced, unbounded duration,
/// ~zero CPU (a socket read parked on a silent peer, a `process.run` waiting
/// on a child, an outbound HTTP call against a slow server). Waits parked on
/// pool threads starve every later job; they get dedicated waiter threads
/// instead, so the pool bound only ever applies to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobClass {
    /// Short, resource-bound work. Runs on the bounded pool.
    Work,
    /// Peer-paced wait of unbounded duration. Runs on its own waiter thread;
    /// never occupies a pool slot.
    Wait,
}

/// Runaway backstop for waiter threads, far above any sane concurrent-wait
/// count. At the cap a `Wait` job degrades to the pool queue (today's
/// behavior) rather than failing.
const WAITER_CAP: usize = 512;

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
    /// Live waiter threads (Wait-class jobs). Only read/written on the
    /// submitting (main) thread for the cap check; decremented by the waiter
    /// itself on exit.
    live_waiters: Arc<AtomicUsize>,
    /// Cap on live waiter threads; above it Wait jobs degrade to the pool.
    waiter_cap: usize,
    workers: Vec<JoinHandle<()>>,
}

impl Boundary {
    pub(crate) fn new(pool_size: usize) -> Self {
        Self::with_waiter_cap(pool_size, WAITER_CAP)
    }

    /// Test seam: a small waiter cap makes the degrade-to-pool path reachable.
    fn with_waiter_cap(pool_size: usize, waiter_cap: usize) -> Self {
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
            live_waiters: Arc::new(AtomicUsize::new(0)),
            waiter_cap,
            workers,
        }
    }

    /// Submit blocking `work` for `task_id`. `work` runs off the main thread
    /// and must touch only owned `Send` data — never the `Store` or guest
    /// memory. `Work` runs on the bounded pool; `Wait` gets a dedicated waiter
    /// thread so a long-lived wait can never starve the pool (plan 103 R1).
    pub(crate) fn submit<F>(&self, task_id: i32, class: JobClass, work: F)
    where
        F: FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        // Cap check happens here, not in spawn_waiter: submissions only occur
        // on the main thread (the boundary is thread-local), so load-then-add
        // cannot race another submitter. A waiter's decrement racing the load
        // can only undercount, never overshoot the cap. At the cap a Wait job
        // degrades to the pool queue (today's behavior) rather than failing.
        if class == JobClass::Wait && self.live_waiters.load(Ordering::SeqCst) < self.waiter_cap {
            self.spawn_waiter(task_id, work);
            return;
        }
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.jobs.push_back((task_id, Box::new(work)));
        }
        self.shared.available.notify_one();
    }

    /// Run `work` on a fresh waiter thread. Waiter threads are detached: they
    /// hold only the completion side, push their result like a pool worker,
    /// and exit. A waiter stuck in an uncancellable syscall at shutdown is
    /// left to the OS rather than joined — the same exposure a stuck pool
    /// worker has today, without wedging teardown. `thread::spawn` consumes
    /// the closure even when it fails, so a (OOM-level) spawn failure is
    /// surfaced as a failed completion — the parked task still resumes.
    fn spawn_waiter<F>(&self, task_id: i32, work: F)
    where
        F: FnOnce() -> Box<dyn Any + Send> + Send + 'static,
    {
        self.live_waiters.fetch_add(1, Ordering::SeqCst);
        let live = Arc::clone(&self.live_waiters);
        let completions = Arc::clone(&self.completions);
        let spawned = thread::Builder::new()
            .name(format!("fai-wait-{task_id}"))
            .spawn(move || {
                let result: JobResult = match catch_unwind(AssertUnwindSafe(work)) {
                    Ok(value) => Ok(value),
                    Err(_) => Err("boundary job panicked".to_string()),
                };
                {
                    let mut q = completions.queue.lock().unwrap();
                    q.push_back(Completion { task_id, result });
                }
                completions.ready.notify_one();
                live.fetch_sub(1, Ordering::SeqCst);
            })
            .is_ok();
        if !spawned {
            self.live_waiters.fetch_sub(1, Ordering::SeqCst);
            {
                let mut q = self.completions.queue.lock().unwrap();
                q.push_back(Completion {
                    task_id,
                    result: Err("failed to spawn boundary waiter thread".to_string()),
                });
            }
            self.completions.ready.notify_one();
        }
    }

    /// Drain every completion ready so far without blocking. Each drained
    /// completion decrements the in-flight count.
    pub(crate) fn drain_completions(&self) -> Vec<Completion> {
        let mut q = self.completions.queue.lock().unwrap();
        let drained: Vec<Completion> = q.drain(..).collect();
        if !drained.is_empty() {
            self.inflight.fetch_sub(drained.len(), Ordering::Relaxed);
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
        let (q, _timed_out) = self.completions.ready.wait_timeout(q, timeout).unwrap();
        !q.is_empty()
    }

    /// Jobs submitted but not yet drained. Zero means no outstanding work.
    pub(crate) fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
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

/// Wait until at least one completion is ready, or until `timeout` elapses.
/// Unlike `pump_ready`, this does not create the boundary pool for programs
/// that never offload work.
pub(crate) fn wait_for_ready(timeout: Duration) -> bool {
    try_with_boundary(|b| b.wait(timeout)).unwrap_or(false)
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
    READY.with(|map| {
        map.borrow_mut().clear();
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
            b.submit(n as i32, JobClass::Work, move || {
                Box::new(n * 10) as Box<dyn Any + Send>
            });
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
            b.submit(n, JobClass::Work, move || {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(20));
                live.fetch_sub(1, Ordering::SeqCst);
                Box::new(()) as Box<dyn Any + Send>
            });
        }
        drain_until(&b, 12, |c| c.len() == 12);
        assert!(
            peak.load(Ordering::SeqCst) <= POOL,
            "peak exceeded pool size"
        );
    }

    #[test]
    fn wait_unblocks_on_completion() {
        let b = Boundary::new(1);
        // Nothing submitted yet: wait should time out (no completion).
        assert!(!b.wait(Duration::from_millis(50)));
        b.submit(7, JobClass::Work, || {
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
        b.submit(3, JobClass::Work, || panic!("boom"));
        // Pool survives and a later job still runs.
        b.submit(4, JobClass::Work, || Box::new(1i64) as Box<dyn Any + Send>);
        let got = drain_until(&b, 2, |c| c.len() == 2);
        let failed = got.iter().find(|c| c.task_id == 3).unwrap();
        assert!(failed.result.is_err());
        let ok = got.iter().find(|c| c.task_id == 4).unwrap();
        assert!(ok.result.is_ok());
    }

    /// Plan 103 R1: long-lived waits must not starve the bounded pool. With
    /// every pool slot's worth of Wait jobs (and more) parked, a Work job
    /// still completes promptly.
    #[test]
    fn wait_jobs_do_not_starve_work_jobs() {
        const POOL: usize = 2;
        let b = Boundary::with_waiter_cap(POOL, WAITER_CAP);
        let release = Arc::new(AtomicUsize::new(0));
        for n in 0..(POOL as i32 + 4) {
            let release = Arc::clone(&release);
            b.submit(n, JobClass::Wait, move || {
                // Peer-paced wait: parked until the test releases it.
                while release.load(Ordering::SeqCst) == 0 {
                    thread::sleep(Duration::from_millis(5));
                }
                Box::new(()) as Box<dyn Any + Send>
            });
        }
        let started = Instant::now();
        b.submit(100, JobClass::Work, || Box::new(42i64) as Box<dyn Any + Send>);
        // The Work job must complete while every Wait job is still parked.
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut work_done = false;
        while Instant::now() < deadline && !work_done {
            b.wait(Duration::from_millis(50));
            work_done = b.drain_completions().iter().any(|c| c.task_id == 100);
        }
        let elapsed = started.elapsed();
        release.store(1, Ordering::SeqCst);
        assert!(
            work_done,
            "Work job starved behind parked Wait jobs ({elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "Work job took {elapsed:?} behind parked Wait jobs"
        );
        drain_until(&b, POOL + 4, |c| c.len() == POOL + 4);
        assert_eq!(b.inflight(), 0);
    }

    /// A panicking Wait job becomes a failed completion, like a pool job.
    #[test]
    fn panicking_wait_job_becomes_failed_completion() {
        let b = Boundary::new(1);
        b.submit(9, JobClass::Wait, || panic!("boom"));
        let got = drain_until(&b, 1, |c| c.len() == 1);
        assert_eq!(got[0].task_id, 9);
        assert!(got[0].result.is_err());
    }

    /// At the waiter cap, Wait jobs degrade to the pool queue (still complete,
    /// never fail) — the runaway backstop keeps correctness.
    #[test]
    fn wait_jobs_degrade_to_pool_at_cap() {
        let b = Boundary::with_waiter_cap(2, 1);
        for n in 0..4i32 {
            b.submit(n, JobClass::Wait, move || {
                thread::sleep(Duration::from_millis(10));
                Box::new(n as i64) as Box<dyn Any + Send>
            });
        }
        let got = drain_until(&b, 4, |c| c.len() == 4);
        let mut ids: Vec<i32> = got.iter().map(|c| c.task_id).collect();
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2, 3]);
        assert_eq!(b.inflight(), 0);
    }

    /// Waiter threads decrement the live count on exit so the cap recovers.
    #[test]
    fn waiter_count_recovers_after_completion() {
        let b = Boundary::with_waiter_cap(1, 2);
        for round in 0..3 {
            for n in 0..2i32 {
                b.submit(round * 10 + n, JobClass::Wait, || {
                    Box::new(()) as Box<dyn Any + Send>
                });
            }
            drain_until(&b, 2, |c| c.len() == 2);
            // Give exiting waiters a beat to decrement.
            let deadline = Instant::now() + Duration::from_secs(1);
            while b.live_waiters.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(b.live_waiters.load(Ordering::SeqCst), 0);
        }
    }
}
