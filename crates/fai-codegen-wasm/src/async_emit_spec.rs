use crate::runtime;
use fai_compiler::ast::{Expression, Program, Statement};

const GLOBAL_RESULT: u32 = 2;
const GLOBAL_STATE: u32 = 0;
const GLOBAL_WAKE_MS: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsyncEmitterSpec {
    pub(crate) frame: AsyncFrameSpec,
    pub(crate) root: AsyncRootTaskSpec,
    pub(crate) children: Vec<AsyncChildTaskSpec>,
    pub(crate) task_records: Vec<AsyncTaskRecord>,
    pub(crate) handlers: AsyncHandlerSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsyncFrameSpec {
    pub(crate) resume_state: ResumeStateSlot,
    pub(crate) root_wake: WakeSlot,
    pub(crate) locals: Vec<FrameLocal>,
    pub(crate) post_wait_locals: Vec<FrameLocal>,
    pub(crate) pending_tasks: Vec<PendingTaskSlot>,
    pub(crate) handler_state: HandlerFrameState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResumeStateSlot {
    pub(crate) global: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WakeSlot {
    pub(crate) global: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingTaskSlot {
    pub(crate) task_id: i32,
    pub(crate) state_global: u32,
    pub(crate) wake_global: u32,
    pub(crate) result_local_idx: usize,
    pub(crate) local_init_start: usize,
    pub(crate) local_init_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsyncTaskRecord {
    pub(crate) slot: PendingTaskSlot,
    pub(crate) child: AsyncChildTaskSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandlerFrameState {
    pub(crate) catch_error_local_idx: Option<usize>,
    pub(crate) catch_local_init_start: usize,
    pub(crate) catch_local_init_count: usize,
    pub(crate) finally_local_init_start: usize,
    pub(crate) finally_local_init_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsyncRootTaskSpec {
    pub(crate) delay_ms: f64,
    pub(crate) complete_immediately: bool,
    pub(crate) result: ResultExpr,
    pub(crate) error: Option<ThrowValue>,
    pub(crate) local_init_start: usize,
    pub(crate) local_init_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsyncChildTaskSpec {
    pub(crate) delay_ms: f64,
    pub(crate) error: Option<ThrowValue>,
    pub(crate) result_local_idx: usize,
    pub(crate) local_init_start: usize,
    pub(crate) local_init_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AsyncHandlerSpec {
    pub(crate) catch_error_local_idx: Option<usize>,
    pub(crate) catch_local_init_start: usize,
    pub(crate) catch_local_init_count: usize,
    pub(crate) catch_result: Option<ResultExpr>,
    /// Narrow-path `finally` support. When `Some`, the finally body
    /// is a single terminal expression that is added (when both
    /// sides are int) to whatever result the try/catch produces
    /// (success, catch, or auto-wait child failure). This mirrors
    /// the language semantic that `finally` runs unconditionally and
    /// contributes to the surrounding value without replacing it.
    /// `None` means the program has no finally or the finally is
    /// not in the narrow supported shape.
    pub(crate) finally_expr: Option<ResultExpr>,
    pub(crate) finally_local_init_start: usize,
    pub(crate) finally_local_init_count: usize,
}

impl AsyncEmitterSpec {
    pub(crate) fn new(
        locals: Vec<FrameLocal>,
        post_wait_locals: Vec<FrameLocal>,
        root: AsyncRootTaskSpec,
        children: Vec<AsyncChildTaskSpec>,
    ) -> Self {
        let frame_local_count = locals.len() + post_wait_locals.len();
        let pending_tasks = pending_task_slots(frame_local_count, children.as_slice());
        let task_records = task_records(pending_tasks.as_slice(), children.as_slice());
        Self {
            frame: AsyncFrameSpec {
                resume_state: ResumeStateSlot {
                    global: GLOBAL_STATE,
                },
                root_wake: WakeSlot {
                    global: GLOBAL_WAKE_MS,
                },
                locals,
                post_wait_locals,
                pending_tasks,
                handler_state: HandlerFrameState::default(),
            },
            root,
            children,
            task_records,
            handlers: AsyncHandlerSpec::default(),
        }
    }

    pub(crate) fn local_global(&self, local_idx: usize) -> u32 {
        frame_local_global(local_idx)
    }

    pub(crate) fn heap_global(&self) -> u32 {
        GLOBAL_RESULT + 1 + self.frame.value_slot_count() as u32 + self.frame.task_slot_count()
    }

    pub(crate) fn sync_frame_handler_state(&mut self) {
        self.frame.pending_tasks =
            pending_task_slots(self.frame.value_slot_count(), self.children.as_slice());
        self.task_records = task_records(
            self.frame.pending_tasks.as_slice(),
            self.children.as_slice(),
        );
        self.frame.handler_state = HandlerFrameState {
            catch_error_local_idx: self.handlers.catch_error_local_idx,
            catch_local_init_start: self.handlers.catch_local_init_start,
            catch_local_init_count: self.handlers.catch_local_init_count,
            finally_local_init_start: self.handlers.finally_local_init_start,
            finally_local_init_count: self.handlers.finally_local_init_count,
        };
    }

    pub(crate) fn first_deferred_local_idx(&self) -> usize {
        let mut first = self.frame.locals.len() + self.frame.post_wait_locals.len();
        if let Some(catch_idx) = self.frame.handler_state.catch_error_local_idx {
            first = first.min(catch_idx);
        }
        if self.frame.handler_state.catch_local_init_count > 0 {
            first = first.min(self.frame.handler_state.catch_local_init_start);
        }
        if self.frame.handler_state.finally_local_init_count > 0 {
            first = first.min(self.frame.handler_state.finally_local_init_start);
        }
        first
    }

    pub(crate) fn child_indices_by_completion(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.children.len()).collect();
        indices.sort_by(|a, b| {
            self.children[*a]
                .delay_ms
                .partial_cmp(&self.children[*b].delay_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b))
        });
        indices
    }

    #[cfg(test)]
    pub(crate) fn child_delays_ms(&self) -> Vec<f64> {
        self.children.iter().map(|child| child.delay_ms).collect()
    }

    #[cfg(test)]
    pub(crate) fn child_errors(&self) -> Vec<Option<ThrowValue>> {
        self.children
            .iter()
            .map(|child| child.error.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn child_result_local_indices(&self) -> Vec<usize> {
        self.children
            .iter()
            .map(|child| child.result_local_idx)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn child_local_init_ranges(&self) -> Vec<(usize, usize)> {
        self.children
            .iter()
            .map(|child| (child.local_init_start, child.local_init_count))
            .collect()
    }
}

impl AsyncFrameSpec {
    pub(crate) fn value_slot_count(&self) -> usize {
        self.locals.len() + self.post_wait_locals.len()
    }

    pub(crate) fn task_slot_count(&self) -> u32 {
        self.pending_tasks.len() as u32 * 2
    }
}

impl Default for HandlerFrameState {
    fn default() -> Self {
        Self {
            catch_error_local_idx: None,
            catch_local_init_start: 0,
            catch_local_init_count: 0,
            finally_local_init_start: 0,
            finally_local_init_count: 0,
        }
    }
}

impl Default for AsyncHandlerSpec {
    fn default() -> Self {
        Self {
            catch_error_local_idx: None,
            catch_local_init_start: 0,
            catch_local_init_count: 0,
            catch_result: None,
            finally_expr: None,
            finally_local_init_start: 0,
            finally_local_init_count: 0,
        }
    }
}

fn pending_task_slots(
    frame_local_count: usize,
    children: &[AsyncChildTaskSpec],
) -> Vec<PendingTaskSlot> {
    let state_base = GLOBAL_RESULT + 1 + frame_local_count as u32;
    let wake_base = state_base + children.len() as u32;
    children
        .iter()
        .enumerate()
        .map(|(idx, child)| PendingTaskSlot {
            task_id: idx as i32 + 2,
            state_global: state_base + idx as u32,
            wake_global: wake_base + idx as u32,
            result_local_idx: child.result_local_idx,
            local_init_start: child.local_init_start,
            local_init_count: child.local_init_count,
        })
        .collect()
}

fn task_records(
    slots: &[PendingTaskSlot],
    children: &[AsyncChildTaskSpec],
) -> Vec<AsyncTaskRecord> {
    slots
        .iter()
        .copied()
        .zip(children.iter().cloned())
        .map(|(slot, child)| AsyncTaskRecord { slot, child })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FrameLocal {
    pub(crate) name: String,
    pub(crate) value: ResultExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IntExpr {
    Literal(i32),
    Local(usize),
    Binary {
        op: IntBinaryOp,
        left: Box<IntExpr>,
        right: Box<IntExpr>,
    },
    TupleIndex {
        tuple_local_idx: usize,
        index: i32,
    },
    /// `left == right` over two boxed strings (object pointers
    /// with `OBJ_TAG_STRING`). Evaluates to `1` if equal, `0`
    /// otherwise. The narrow emitter inlines a byte-by-byte
    /// compare so it doesn't depend on the runtime.
    StringEq {
        left: Box<StringExpr>,
        right: Box<StringExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntBinaryOp {
    Add,
    Sub,
    Mul,
}

/// Narrow-path expression that can appear in a result slot (success
/// value, catch body, finally body). Boxes to a NaN-boxed i64: int
/// for the `Int` variant, object pointer for `String`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ResultExpr {
    Int(IntExpr),
    String(StringExpr),
    Tuple(Vec<ResultExpr>),
    /// `if condition then a else b`: both branches are evaluated
    /// into a boxed i64 result. The condition is an int (0/1).
    /// Both branches may be any `ResultExpr`; mismatched types
    /// are fine at the wasm level (everything is i64).
    IfElse {
        condition: Box<IntExpr>,
        then_branch: Box<ResultExpr>,
        else_branch: Box<ResultExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StringExpr {
    /// Baked string constant. The narrow emitter pushes a
    /// pre-computed boxed pointer; the string bytes live in the
    /// module's data section.
    Literal(String),
    /// `e.message`: the catch body's only allowed field access.
    ErrorMessage,
}

/// Throw-site value used in the narrow path. `ErrorDict` covers
/// `throw Error('msg')` so the catch can inspect the message
/// field. The build step bakes one Error dict per unique message
/// in the program; the throw site pushes the dict for its
/// specific message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ThrowValue {
    IntLiteral(i32),
    ErrorDict(String),
}

pub(crate) fn frame_local_global(local_idx: usize) -> u32 {
    GLOBAL_RESULT + 1 + local_idx as u32
}

pub(crate) fn boxed_obj_ptr(addr: u32) -> i64 {
    (runtime::QNAN as u64 | runtime::SIGN_BIT as u64 | addr as u64) as i64
}

/// Walk the program collecting every unique `Error('msg')` literal
/// in first-seen order. The async emitter bakes a dict for each
/// unique message and the throw site picks the matching one. An
/// empty `Vec` means the program has no `Error(...)` throws.
pub(crate) fn collect_unique_error_messages(ast: &Program) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    visit_program_for_error_message(ast, &mut found);
    found
}

fn visit_program_for_error_message(program: &Program, found: &mut Vec<String>) {
    for stmt in &program.statements {
        visit_statement_for_error_message(stmt, found);
    }
}

fn visit_statement_for_error_message(stmt: &Statement, found: &mut Vec<String>) {
    match stmt {
        Statement::ThrowStatement(ts) => {
            if let Some(message) = error_message_from_call(&ts.expression) {
                if !found.contains(&message) {
                    found.push(message);
                }
            }
        }
        Statement::TryStatement(ts) => {
            for s in &ts.try_body {
                visit_statement_for_error_message(s, found);
            }
            for s in &ts.catch_body {
                visit_statement_for_error_message(s, found);
            }
            if let Some(fb) = &ts.finally_body {
                for s in fb {
                    visit_statement_for_error_message(s, found);
                }
            }
        }
        Statement::IfStatement(is) => {
            for branch in &is.branches {
                for s in &branch.body {
                    visit_statement_for_error_message(s, found);
                }
            }
            if let Some(else_body) = &is.else_branch {
                for s in else_body {
                    visit_statement_for_error_message(s, found);
                }
            }
        }
        Statement::ForStatement(fs) => {
            for s in &fs.body {
                visit_statement_for_error_message(s, found);
            }
        }
        Statement::WhileStatement(ws) => {
            for s in &ws.body {
                visit_statement_for_error_message(s, found);
            }
        }
        Statement::CaseStatement(cs) => {
            for branch in &cs.when_branches {
                for s in &branch.body {
                    visit_statement_for_error_message(s, found);
                }
            }
            if let Some(default) = &cs.default_branch {
                for s in default {
                    visit_statement_for_error_message(s, found);
                }
            }
        }
        Statement::FunctionDeclaration(fd) => {
            for s in &fd.body {
                visit_statement_for_error_message(s, found);
            }
        }
        Statement::ExpressionStatement(_)
        | Statement::LetStatement(_)
        | Statement::VarStatement(_)
        | Statement::AssignmentStatement(_)
        | Statement::ReturnStatement(_)
        | Statement::BreakStatement(_)
        | Statement::ContinueStatement(_)
        | Statement::NowaitStatement(_)
        | Statement::FunctionTypeDefDeclaration(_)
        | Statement::UseStatement(_)
        | Statement::TypeDeclaration(_)
        | Statement::EnumDeclaration(_)
        | Statement::TestDeclaration(_)
        | Statement::ExternBlockDeclaration(_) => {}
    }
}

fn error_message_from_call(expr: &Expression) -> Option<String> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(callee) = &*call.callee else {
        return None;
    };
    if callee.name != "Error" {
        return None;
    }
    let [arg] = call.args.as_slice() else {
        return None;
    };
    let Expression::StringExpression(s) = &arg.value else {
        return None;
    };
    Some(s.value.clone())
}
