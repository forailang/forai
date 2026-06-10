//! Scheduler model for real async lowering.
//!
//! This module intentionally stays host-independent. It models task state,
//! ready queue transitions, timers, and parent wakeups so the compiler's
//! future guest-runtime lowering has a small, tested contract to target.

use std::collections::{HashMap, VecDeque};

pub type TaskId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Ready,
    Running,
    Waiting,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEntry {
    Function { function_index: u32 },
    Closure { closure_value: i64 },
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: TaskId,
    pub status: TaskStatus,
    pub entry: TaskEntry,
    pub frame_ptr: Option<u32>,
    pub result: Option<i64>,
    pub error: Option<i64>,
    pub waiters: Vec<TaskId>,
    /// Absolute time (ms, same clock as `host_now_ms`) at which a
    /// timer-suspended task becomes ready. `None` unless sleeping.
    pub wake_at_ms: Option<u64>,
    /// Number of child tasks this task is still joining on before it
    /// resumes. 1 for a plain auto-awaited call, N for `all(...)`, 0
    /// when not joining. Each child completion/failure decrements it;
    /// the task is made ready when it reaches 0.
    pub join_remaining: u32,
}

/// Status returned by [`Scheduler::poll`], mirroring the guest
/// `__fai_poll` contract: `2` = idle/root-complete, `3` = root-failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
    /// Ready tasks remain or timers are pending; keep pumping.
    Working,
    /// Root task completed successfully; scheduler is idle.
    RootComplete,
    /// Root task failed; scheduler is idle.
    RootFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerEvent {
    TimerRequested { task_id: TaskId, ms: u32 },
    TaskReady(TaskId),
    TaskCompleted(TaskId),
    TaskFailed(TaskId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    UnknownTask(TaskId),
    TaskNotRunning(TaskId),
    TaskAlreadyFinished(TaskId),
}

#[derive(Debug, Default)]
pub struct Scheduler {
    next_task_id: TaskId,
    tasks: HashMap<TaskId, Task>,
    ready: VecDeque<TaskId>,
    events: Vec<SchedulerEvent>,
    /// The root task (`main`). Its completion/failure determines the
    /// program's terminal status; `None` until [`Scheduler::set_root`].
    root: Option<TaskId>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(&mut self, entry: TaskEntry) -> TaskId {
        let id = self.next_task_id;
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .expect("async task id overflow");
        self.tasks.insert(
            id,
            Task {
                id,
                status: TaskStatus::Ready,
                entry,
                frame_ptr: None,
                result: None,
                error: None,
                waiters: Vec::new(),
                wake_at_ms: None,
                join_remaining: 0,
            },
        );
        self.ready.push_back(id);
        self.events.push(SchedulerEvent::TaskReady(id));
        id
    }

    /// Mark `task_id` as the root task whose terminal state drives the
    /// program's exit status (see [`Scheduler::poll`]).
    pub fn set_root(&mut self, task_id: TaskId) {
        self.root = Some(task_id);
    }

    /// Suspend `task_id` on a timer that becomes ready at absolute time
    /// `wake_at_ms` (same clock as `host_now_ms`). This is how `sleep`
    /// lowers: the guest records the deadline and returns to the
    /// scheduler; [`Scheduler::poll`] promotes it when the clock passes.
    pub fn wait_until(
        &mut self,
        task_id: TaskId,
        frame_ptr: u32,
        wake_at_ms: u64,
    ) -> Result<(), SchedulerError> {
        let task = self.running_task_mut(task_id)?;
        task.status = TaskStatus::Waiting;
        task.frame_ptr = Some(frame_ptr);
        task.wake_at_ms = Some(wake_at_ms);
        Ok(())
    }

    /// Suspend `parent` until all `children` finish. `join_remaining`
    /// is set to the count of not-yet-finished children; each child's
    /// completion/failure decrements it and wakes the parent at zero.
    /// Covers both a plain auto-awaited call (one child) and `all(...)`
    /// (N children). Children that already finished count immediately.
    pub fn join_on(
        &mut self,
        parent: TaskId,
        children: &[TaskId],
        frame_ptr: u32,
    ) -> Result<(), SchedulerError> {
        for &child in children {
            if !self.tasks.contains_key(&child) {
                return Err(SchedulerError::UnknownTask(child));
            }
        }
        let remaining = {
            let mut pending = 0u32;
            for &child in children {
                let finished = matches!(
                    self.tasks[&child].status,
                    TaskStatus::Complete | TaskStatus::Failed
                );
                if !finished {
                    pending += 1;
                    let c = self.tasks.get_mut(&child).unwrap();
                    if !c.waiters.contains(&parent) {
                        c.waiters.push(parent);
                    }
                }
            }
            pending
        };
        let task = self.running_task_mut(parent)?;
        task.frame_ptr = Some(frame_ptr);
        task.join_remaining = remaining;
        if remaining == 0 {
            // All children already finished — stay ready to resume.
            task.status = TaskStatus::Ready;
            self.ready.push_back(parent);
            self.events.push(SchedulerEvent::TaskReady(parent));
        } else {
            task.status = TaskStatus::Waiting;
        }
        Ok(())
    }

    /// Drive one scheduler step at clock `now_ms`: promote any timer
    /// whose deadline has passed to ready, then report terminal status.
    /// Mirrors the guest `__fai_poll` contract (idle vs root done).
    pub fn poll(&mut self, now_ms: u64) -> PollStatus {
        let due: Vec<TaskId> = self
            .tasks
            .values()
            .filter(|t| {
                t.status == TaskStatus::Waiting
                    && t.wake_at_ms.map(|w| w <= now_ms).unwrap_or(false)
            })
            .map(|t| t.id)
            .collect();
        for id in due {
            if let Some(t) = self.tasks.get_mut(&id) {
                t.wake_at_ms = None;
            }
            let _ = self.mark_ready(id);
        }

        if let Some(root) = self.root {
            match self.tasks.get(&root).map(|t| t.status) {
                Some(TaskStatus::Complete) => return PollStatus::RootComplete,
                Some(TaskStatus::Failed) => return PollStatus::RootFailed,
                _ => {}
            }
        }
        PollStatus::Working
    }

    /// The earliest pending timer deadline, if any — lets a host event
    /// loop sleep until the next wakeup instead of busy-polling.
    pub fn next_wake_ms(&self) -> Option<u64> {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Waiting)
            .filter_map(|t| t.wake_at_ms)
            .min()
    }

    pub fn pop_ready(&mut self) -> Option<TaskId> {
        while let Some(id) = self.ready.pop_front() {
            if let Some(task) = self.tasks.get_mut(&id) {
                if task.status == TaskStatus::Ready {
                    task.status = TaskStatus::Running;
                    return Some(id);
                }
            }
        }
        None
    }

    pub fn wait_for_timer(
        &mut self,
        task_id: TaskId,
        frame_ptr: u32,
        ms: u32,
    ) -> Result<(), SchedulerError> {
        let task = self.running_task_mut(task_id)?;
        task.status = TaskStatus::Waiting;
        task.frame_ptr = Some(frame_ptr);
        self.events
            .push(SchedulerEvent::TimerRequested { task_id, ms });
        Ok(())
    }

    pub fn add_waiter(
        &mut self,
        child_id: TaskId,
        waiter_id: TaskId,
    ) -> Result<(), SchedulerError> {
        if !self.tasks.contains_key(&waiter_id) {
            return Err(SchedulerError::UnknownTask(waiter_id));
        }
        let child = self
            .tasks
            .get_mut(&child_id)
            .ok_or(SchedulerError::UnknownTask(child_id))?;
        match child.status {
            TaskStatus::Complete | TaskStatus::Failed => self.mark_ready(waiter_id),
            _ => {
                if !child.waiters.contains(&waiter_id) {
                    child.waiters.push(waiter_id);
                }
                Ok(())
            }
        }
    }

    pub fn resume_task(&mut self, task_id: TaskId) -> Result<(), SchedulerError> {
        self.mark_ready(task_id)
    }

    pub fn complete(&mut self, task_id: TaskId, value: i64) -> Result<(), SchedulerError> {
        let waiters = {
            let task = self.running_task_mut(task_id)?;
            task.status = TaskStatus::Complete;
            task.result = Some(value);
            task.frame_ptr = None;
            std::mem::take(&mut task.waiters)
        };
        self.events.push(SchedulerEvent::TaskCompleted(task_id));
        for waiter in waiters {
            self.notify_waiter(waiter)?;
        }
        Ok(())
    }

    pub fn fail(&mut self, task_id: TaskId, error: i64) -> Result<(), SchedulerError> {
        let waiters = {
            let task = self.running_task_mut(task_id)?;
            task.status = TaskStatus::Failed;
            task.error = Some(error);
            task.frame_ptr = None;
            std::mem::take(&mut task.waiters)
        };
        self.events.push(SchedulerEvent::TaskFailed(task_id));
        for waiter in waiters {
            self.notify_waiter(waiter)?;
        }
        Ok(())
    }

    /// A child this waiter was joining on finished. Decrement the
    /// outstanding join count; when it hits zero the waiter is ready to
    /// resume. A waiter with `join_remaining == 0` (legacy single-wait
    /// callers via [`Scheduler::add_waiter`]) is woken immediately.
    fn notify_waiter(&mut self, waiter: TaskId) -> Result<(), SchedulerError> {
        let ready_now = {
            let task = self
                .tasks
                .get_mut(&waiter)
                .ok_or(SchedulerError::UnknownTask(waiter))?;
            if task.join_remaining > 0 {
                task.join_remaining -= 1;
            }
            task.join_remaining == 0
        };
        if ready_now {
            self.mark_ready(waiter)?;
        }
        Ok(())
    }

    pub fn task(&self, task_id: TaskId) -> Option<&Task> {
        self.tasks.get(&task_id)
    }

    pub fn drain_events(&mut self) -> Vec<SchedulerEvent> {
        std::mem::take(&mut self.events)
    }

    fn running_task_mut(&mut self, task_id: TaskId) -> Result<&mut Task, SchedulerError> {
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or(SchedulerError::UnknownTask(task_id))?;
        match task.status {
            TaskStatus::Running => Ok(task),
            TaskStatus::Complete | TaskStatus::Failed => {
                Err(SchedulerError::TaskAlreadyFinished(task_id))
            }
            _ => Err(SchedulerError::TaskNotRunning(task_id)),
        }
    }

    fn mark_ready(&mut self, task_id: TaskId) -> Result<(), SchedulerError> {
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or(SchedulerError::UnknownTask(task_id))?;
        match task.status {
            TaskStatus::Complete | TaskStatus::Failed => {
                Err(SchedulerError::TaskAlreadyFinished(task_id))
            }
            TaskStatus::Ready => Ok(()),
            TaskStatus::Running | TaskStatus::Waiting => {
                task.status = TaskStatus::Ready;
                self.ready.push_back(task_id);
                self.events.push(SchedulerEvent::TaskReady(task_id));
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(index: u32) -> TaskEntry {
        TaskEntry::Function {
            function_index: index,
        }
    }

    #[test]
    fn spawned_task_enters_ready_queue_and_runs_once() {
        let mut scheduler = Scheduler::new();
        let task = scheduler.spawn(function(7));

        assert_eq!(scheduler.pop_ready(), Some(task));
        assert_eq!(scheduler.task(task).unwrap().status, TaskStatus::Running);
        assert_eq!(scheduler.pop_ready(), None);
    }

    #[test]
    fn running_task_can_wait_for_timer_and_resume() {
        let mut scheduler = Scheduler::new();
        let task = scheduler.spawn(function(1));
        assert_eq!(scheduler.pop_ready(), Some(task));

        scheduler.wait_for_timer(task, 1024, 25).unwrap();
        assert_eq!(scheduler.task(task).unwrap().status, TaskStatus::Waiting);
        assert_eq!(scheduler.task(task).unwrap().frame_ptr, Some(1024));
        assert_eq!(
            scheduler.drain_events().last(),
            Some(&SchedulerEvent::TimerRequested {
                task_id: task,
                ms: 25
            })
        );

        scheduler.resume_task(task).unwrap();
        assert_eq!(scheduler.pop_ready(), Some(task));
    }

    #[test]
    fn completing_child_wakes_waiting_parent() {
        let mut scheduler = Scheduler::new();
        let parent = scheduler.spawn(function(1));
        let child = scheduler.spawn(function(2));
        assert_eq!(scheduler.pop_ready(), Some(parent));
        scheduler.wait_for_timer(parent, 2048, 100).unwrap();
        scheduler.add_waiter(child, parent).unwrap();

        assert_eq!(scheduler.pop_ready(), Some(child));
        scheduler.complete(child, 42).unwrap();

        assert_eq!(scheduler.task(child).unwrap().result, Some(42));
        assert_eq!(scheduler.pop_ready(), Some(parent));
    }

    #[test]
    fn failing_child_wakes_waiting_parent() {
        let mut scheduler = Scheduler::new();
        let parent = scheduler.spawn(function(1));
        let child = scheduler.spawn(function(2));
        assert_eq!(scheduler.pop_ready(), Some(parent));
        scheduler.wait_for_timer(parent, 2048, 100).unwrap();
        scheduler.add_waiter(child, parent).unwrap();

        assert_eq!(scheduler.pop_ready(), Some(child));
        scheduler.fail(child, 99).unwrap();

        assert_eq!(scheduler.task(child).unwrap().error, Some(99));
        assert_eq!(scheduler.pop_ready(), Some(parent));
    }

    #[test]
    fn poll_promotes_a_timer_once_its_deadline_passes() {
        let mut scheduler = Scheduler::new();
        let task = scheduler.spawn(function(1));
        scheduler.set_root(task);
        assert_eq!(scheduler.pop_ready(), Some(task));

        scheduler.wait_until(task, 4096, 100).unwrap();
        assert_eq!(scheduler.task(task).unwrap().status, TaskStatus::Waiting);

        // Before the deadline: still waiting, nothing ready.
        assert_eq!(scheduler.poll(50), PollStatus::Working);
        assert_eq!(scheduler.next_wake_ms(), Some(100));
        assert_eq!(scheduler.pop_ready(), None);

        // At/after the deadline: promoted to ready.
        assert_eq!(scheduler.poll(100), PollStatus::Working);
        assert_eq!(scheduler.pop_ready(), Some(task));
    }

    #[test]
    fn single_child_join_resumes_parent_on_completion() {
        let mut scheduler = Scheduler::new();
        let parent = scheduler.spawn(function(1));
        let child = scheduler.spawn(function(2));
        assert_eq!(scheduler.pop_ready(), Some(parent));
        let _ = scheduler.pop_ready(); // child runs

        scheduler.join_on(parent, &[child], 2048).unwrap();
        assert_eq!(scheduler.task(parent).unwrap().status, TaskStatus::Waiting);

        scheduler.complete(child, 7).unwrap();
        assert_eq!(scheduler.pop_ready(), Some(parent));
    }

    #[test]
    fn all_join_resumes_parent_only_after_every_child_finishes() {
        let mut scheduler = Scheduler::new();
        let parent = scheduler.spawn(function(1));
        let a = scheduler.spawn(function(2));
        let b = scheduler.spawn(function(3));
        assert_eq!(scheduler.pop_ready(), Some(parent));
        let _ = scheduler.pop_ready(); // a
        let _ = scheduler.pop_ready(); // b

        scheduler.join_on(parent, &[a, b], 2048).unwrap();
        assert_eq!(scheduler.task(parent).unwrap().join_remaining, 2);

        // First child done — parent still waiting.
        scheduler.complete(a, 1).unwrap();
        assert_eq!(scheduler.task(parent).unwrap().status, TaskStatus::Waiting);
        assert_eq!(scheduler.pop_ready(), None);

        // Second child done — parent becomes ready.
        scheduler.complete(b, 2).unwrap();
        assert_eq!(scheduler.pop_ready(), Some(parent));
    }

    #[test]
    fn join_on_already_finished_children_keeps_parent_ready() {
        let mut scheduler = Scheduler::new();
        let parent = scheduler.spawn(function(1));
        let child = scheduler.spawn(function(2));
        assert_eq!(scheduler.pop_ready(), Some(parent));
        assert_eq!(scheduler.pop_ready(), Some(child));
        scheduler.complete(child, 5).unwrap();

        // Child already complete before the parent joins: no suspension.
        scheduler.join_on(parent, &[child], 2048).unwrap();
        assert_eq!(scheduler.task(parent).unwrap().status, TaskStatus::Ready);
        assert_eq!(scheduler.pop_ready(), Some(parent));
    }

    #[test]
    fn poll_reports_root_terminal_status() {
        let mut scheduler = Scheduler::new();
        let root = scheduler.spawn(function(1));
        scheduler.set_root(root);
        assert_eq!(scheduler.pop_ready(), Some(root));
        assert_eq!(scheduler.poll(0), PollStatus::Working);

        scheduler.complete(root, 42).unwrap();
        assert_eq!(scheduler.poll(0), PollStatus::RootComplete);
    }

    #[test]
    fn poll_reports_root_failure() {
        let mut scheduler = Scheduler::new();
        let root = scheduler.spawn(function(1));
        scheduler.set_root(root);
        assert_eq!(scheduler.pop_ready(), Some(root));
        scheduler.fail(root, 1).unwrap();
        assert_eq!(scheduler.poll(0), PollStatus::RootFailed);
    }
}
