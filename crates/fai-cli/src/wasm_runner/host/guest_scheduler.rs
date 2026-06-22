//! Cached guest scheduler exports for host-side driver loops.

use wasmtime::{Caller, Func, Global, Val};

use super::super::nan_box::VAL_NULL;

pub(super) struct GuestScheduler {
    poll: Option<Func>,
    resume_task: Option<Func>,
    spawn_closure: Option<Func>,
    spawn_queued_closure: Option<Func>,
    pop_completed_task: Option<Func>,
    task_status: Option<Func>,
    task_result: Option<Func>,
    free_task: Option<Func>,
    live: Option<Global>,
}

impl GuestScheduler {
    pub(super) fn new(caller: &mut Caller<'_, ()>) -> Self {
        Self {
            poll: func(caller, "__fai_poll"),
            resume_task: func(caller, "__fai_resume_task"),
            spawn_closure: func(caller, "__fai_spawn_closure"),
            spawn_queued_closure: func(caller, "__fai_spawn_queued_closure"),
            pop_completed_task: func(caller, "__fai_pop_completed_task"),
            task_status: func(caller, "__fai_task_status"),
            task_result: func(caller, "__fai_task_result"),
            free_task: func(caller, "__fai_free_task"),
            live: caller
                .get_export("__dbg_live")
                .and_then(|e| e.into_global()),
        }
    }

    pub(super) fn poll(&self, caller: &mut Caller<'_, ()>) -> i32 {
        let Some(f) = &self.poll else {
            return 0;
        };
        let mut out = [Val::I32(0)];
        if f.call(&mut *caller, &[], &mut out).is_err() {
            return 0;
        }
        match out[0] {
            Val::I32(v) => v,
            _ => 0,
        }
    }

    pub(super) fn resume_task(&self, caller: &mut Caller<'_, ()>, id: i32) {
        if let Some(f) = &self.resume_task {
            let _ = f.call(&mut *caller, &[Val::I32(id)], &mut [Val::I32(0)]);
        }
    }

    pub(super) fn spawn_closure(
        &self,
        caller: &mut Caller<'_, ()>,
        handler: i64,
        arg: i64,
    ) -> Option<i32> {
        self.spawn_with(caller, self.spawn_closure.as_ref(), handler, arg)
    }

    pub(super) fn spawn_queued_closure(
        &self,
        caller: &mut Caller<'_, ()>,
        handler: i64,
        arg: i64,
    ) -> Option<i32> {
        self.spawn_with(caller, self.spawn_queued_closure.as_ref(), handler, arg)
            .or_else(|| self.spawn_closure(caller, handler, arg))
    }

    pub(super) fn has_completed_queue(&self) -> bool {
        self.pop_completed_task.is_some()
    }

    pub(super) fn pop_completed_task(&self, caller: &mut Caller<'_, ()>) -> Option<i32> {
        let f = self.pop_completed_task.as_ref()?;
        let mut out = [Val::I32(-1)];
        if f.call(&mut *caller, &[], &mut out).is_err() {
            return None;
        }
        match out[0] {
            Val::I32(v) if v >= 0 => Some(v),
            _ => None,
        }
    }

    pub(super) fn task_status(&self, caller: &mut Caller<'_, ()>, id: i32) -> i32 {
        const ST_FAILED: i32 = 4;
        let Some(f) = &self.task_status else {
            return ST_FAILED;
        };
        let mut out = [Val::I32(0)];
        if f.call(&mut *caller, &[Val::I32(id)], &mut out).is_err() {
            return ST_FAILED;
        }
        match out[0] {
            Val::I32(v) => v,
            _ => ST_FAILED,
        }
    }

    pub(super) fn task_result(&self, caller: &mut Caller<'_, ()>, id: i32) -> i64 {
        let Some(f) = &self.task_result else {
            return VAL_NULL;
        };
        let mut out = [Val::I64(0)];
        if f.call(&mut *caller, &[Val::I32(id)], &mut out).is_err() {
            return VAL_NULL;
        }
        match out[0] {
            Val::I64(v) => v,
            _ => VAL_NULL,
        }
    }

    pub(super) fn free_task(&self, caller: &mut Caller<'_, ()>, id: i32) {
        if let Some(f) = &self.free_task {
            let _ = f.call(&mut *caller, &[Val::I32(id)], &mut []);
        }
    }

    pub(super) fn live_count(&self, caller: &mut Caller<'_, ()>) -> i32 {
        self.live
            .as_ref()
            .and_then(|g| match g.get(&mut *caller) {
                Val::I32(v) => Some(v),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn spawn_with(
        &self,
        caller: &mut Caller<'_, ()>,
        func: Option<&Func>,
        handler: i64,
        arg: i64,
    ) -> Option<i32> {
        let f = func?;
        let mut out = [Val::I64(0)];
        f.call(&mut *caller, &[Val::I64(handler), Val::I64(arg)], &mut out)
            .ok()?;
        match out[0] {
            Val::I64(v) => Some(v as i32),
            _ => None,
        }
    }
}

fn func(caller: &mut Caller<'_, ()>, name: &str) -> Option<Func> {
    caller.get_export(name).and_then(|e| e.into_func())
}
