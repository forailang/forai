use super::*;

impl<'a, 'c> Builder<'a, 'c> {
    pub(super) fn new(
        fd: &'a FunctionDeclaration,
        ctx: &'c BuildContext<'a>,
        outer_scope: Option<&'c OuterScopeView<'c>>,
    ) -> Self {
        // Map each user parameter to its corresponding wasm local
        // index. Type params (generic `@type T`) come FIRST because
        // the call-site emission pushes type-arg strings before the
        // real user args (see `compile_call`), so they land in the
        // callee's lowest locals. Binding user params first here
        // would alias every user param to the wrong wasm local —
        // generic calls then read back the type-arg string instead
        // of the user value.
        let mut first_scope = HashMap::new();
        let mut idx = 0u32;
        for t in &fd.type_params {
            first_scope.insert(
                t.name.clone(),
                LocalBinding {
                    local: idx,
                    shape: ValueShape::Boxed,
                    is_cell: false,
                },
            );
            idx += 1;
        }
        for p in &fd.params {
            first_scope.insert(
                p.name.clone(),
                LocalBinding {
                    local: idx,
                    shape: ValueShape::Boxed,
                    is_cell: false,
                },
            );
            idx += 1;
        }
        Self {
            fd,
            ctx,
            instrs: Vec::new(),
            next_local: idx,
            scopes: vec![first_scope],
            scope_drops: vec![Vec::new()],
            confined_escaping: fai_compiler::escape_analysis::conservative_escaping(fd),
            local_decls: Vec::new(),
            loops: Vec::new(),
            tries: Vec::new(),
            block_depth: 0,
            function_by_name: ctx
                .functions
                .iter()
                .enumerate()
                .map(|(i, f)| (f.name.clone(), i as u32))
                .collect(),
            // Phase E start: all tests drive the entry module, so key
            // is the empty string. Nested modules will populate this
            // when the caller sets it via `compile_prepared_with_…`.
            module_key: String::new(),
            module_context: None,
            outer_scope,
            upvalues: Vec::new(),
            upvalue_by_name: HashMap::new(),
            cell_captured_vars: collect_cell_captured_vars(&fd.body),
            owned_frame_locals: HashSet::new(),
            async_error_ctx: None,
        }
    }

    pub(super) fn rt(&self) -> RtOffsets {
        self.ctx.rt
    }

    /// Emit a `Call` to a host import using the target-aware remap.
    /// If the import is available for the current target, the call lands on
    /// its remapped wasm function index. If not, emit `unreachable` — matches
    /// `runtime::emit_import_call`'s policy so both codegen paths trap
    /// identically on unavailable imports.
    pub(super) fn emit_import_call(&mut self, import_idx: u32) {
        match self
            .ctx
            .import_remap
            .get(import_idx as usize)
            .copied()
            .flatten()
        {
            Some(new_idx) => {
                self.emit(Instruction::Call(new_idx));
            }
            None => {
                self.emit(Instruction::Unreachable);
            }
        }
    }

    fn ownership_events_enabled(&self) -> bool {
        self.ctx
            .import_remap
            .get(IMPORT_OWNERSHIP_EVENT as usize)
            .copied()
            .flatten()
            .is_some()
    }

    fn debug_function_calls_enabled(&self) -> bool {
        self.ctx
            .import_remap
            .get(crate::runtime::IMPORT_DEBUG_FUNCTION_CALL as usize)
            .copied()
            .flatten()
            .is_some()
    }

    fn emit_debug_function_event(&mut self, event: i32) {
        if !self.debug_function_calls_enabled() {
            return;
        }
        let (off, len) = {
            let mut strings = self.ctx.strings.borrow_mut();
            strings.intern(&self.fd.name)
        };
        self.emit(Instruction::I32Const(off as i32));
        self.emit(Instruction::I32Const(len as i32));
        self.emit(Instruction::I32Const(event));
        self.emit_import_call(crate::runtime::IMPORT_DEBUG_FUNCTION_CALL);
    }

    fn emit_debug_function_start(&mut self) {
        self.emit_debug_function_event(0);
    }

    fn emit_debug_function_end(&mut self) {
        self.emit_debug_function_event(1);
    }

    pub(super) fn current_source_file(&self) -> Option<String> {
        if let Some(file) = self.ctx.file_path {
            return Some(file.to_string());
        }
        if !self.module_key.is_empty() && self.module_key.contains('/') {
            return Some(self.module_key.clone());
        }
        self.ctx
            .functions
            .iter()
            .find(|info| info.name == self.fd.name)
            .and_then(|info| info.source_file.clone())
    }

    fn ownership_site(&mut self, op: OwnershipOp, site_id: u32) -> u32 {
        if site_id != OWNERSHIP_SITE_UNKNOWN {
            return site_id;
        }
        let mut sites = self.ctx.ownership_sites.borrow_mut();
        let id = sites.len() as u32 + 1;
        sites.push(crate::debug_info::OwnershipSiteDebugEntry {
            id,
            op: op.name(),
            helper: "direct",
            reason: ownership_reason(op),
            file: self.current_source_file(),
            line: self.fd.location.line,
        });
        id
    }

    #[allow(dead_code)]
    fn emit_ownership_event_const(&mut self, op: OwnershipOp, site_id: u32, value: i64, aux: i32) {
        if !self.ownership_events_enabled() {
            return;
        }
        if ownership_seed_suppresses(op) {
            return;
        }
        let site_id = self.ownership_site(op, site_id);
        self.emit(Instruction::I32Const(op.id() as i32));
        self.emit(Instruction::I32Const(site_id as i32));
        self.emit(Instruction::I64Const(value));
        self.emit(Instruction::I32Const(aux));
        self.emit_import_call(IMPORT_OWNERSHIP_EVENT);
    }

    pub(super) fn emit_ownership_event_for_stack(&mut self, op: OwnershipOp, site_id: u32, aux: i32) {
        if !self.ownership_events_enabled() {
            return;
        }
        if ownership_seed_suppresses(op) {
            return;
        }
        let site_id = self.ownership_site(op, site_id);
        let value = self.alloc_local();
        self.emit(Instruction::LocalTee(value));
        self.emit(Instruction::I32Const(op.id() as i32));
        self.emit(Instruction::I32Const(site_id as i32));
        self.emit(Instruction::LocalGet(value));
        self.emit(Instruction::I32Const(aux));
        self.emit_import_call(IMPORT_OWNERSHIP_EVENT);
    }

    pub(super) fn emit_ownership_event_for_local(
        &mut self,
        op: OwnershipOp,
        site_id: u32,
        value_local: u32,
        aux: i32,
    ) {
        if !self.ownership_events_enabled() {
            return;
        }
        if ownership_seed_suppresses(op) {
            return;
        }
        let site_id = self.ownership_site(op, site_id);
        self.emit(Instruction::I32Const(op.id() as i32));
        self.emit(Instruction::I32Const(site_id as i32));
        self.emit(Instruction::LocalGet(value_local));
        self.emit(Instruction::I32Const(aux));
        self.emit_import_call(IMPORT_OWNERSHIP_EVENT);
    }

    pub(super) fn functions(&self) -> &'a [FunctionInfo] {
        self.ctx.functions
    }

    pub(super) fn checker(&self) -> &'a CheckerInfo {
        self.ctx.checker
    }

    pub(super) fn expression_type_at(&self, expr: &Expression) -> Option<&fai_checker::types::Type> {
        let key = fai_checker::checker::expression_key(expr, self.module_key.clone());
        self.checker().expression_types.get(&key)
    }

    pub(super) fn shape_for_expr(&self, expr: &Expression) -> ValueShape {
        self.expression_type_at(expr)
            .map(shape_for_type)
            .unwrap_or(ValueShape::Boxed)
    }

    pub(super) fn numeric_shape_for_expr(&self, expr: &Expression) -> Option<ValueShape> {
        match expr {
            Expression::NumberExpression(n) => {
                if !n.is_float && n.value == (n.value as i64) as f64 {
                    Some(ValueShape::RawInt)
                } else {
                    Some(ValueShape::RawFloat)
                }
            }
            Expression::IdentifierExpression(id) => {
                // A binding physically stored in a raw numeric local is
                // trivially that shape.
                if let Some(binding) = self.lookup(&id.name) {
                    if matches!(binding.shape, ValueShape::RawInt | ValueShape::RawFloat) {
                        return Some(binding.shape);
                    }
                }
                // Otherwise fall back to the checker's static type: a
                // Boxed binding the checker proved Int/Float still holds a
                // scalar payload, and unboxing it (`compile_expr_as(_,
                // RawInt)` = `wrap; extend_s`) is sound. This lets native
                // arithmetic apply to boxed-but-scalar bindings — e.g. a
                // `for x in ints` loop variable, whose `total + x` would
                // otherwise pay a `rt_add` call every iteration instead of
                // a native `i64.add`.
                match self.shape_for_expr(expr) {
                    shape @ (ValueShape::RawInt | ValueShape::RawFloat) => Some(shape),
                    _ => None,
                }
            }
            _ => match self.shape_for_expr(expr) {
                shape @ (ValueShape::RawInt | ValueShape::RawFloat) => Some(shape),
                _ => None,
            },
        }
    }

    /// True when `ce` is the bare builtin `length(x)` — callee is the
    /// identifier `length`, one argument, and `length` is neither a
    /// local binding nor a user function (the same conditions under
    /// which `compile_call` routes it to `try_compile_bare_global`).
    fn is_bare_length_call(&self, ce: &CallExpression) -> bool {
        matches!(&*ce.callee, Expression::IdentifierExpression(id) if id.name == "length")
            && ce.args.len() == 1
            && self.lookup("length").is_none()
            && !self.resolves_to_user_fn("length")
    }

    pub(super) fn compile_expr_as(&mut self, expr: &Expression, want: ValueShape) -> Result<(), BuildError> {
        // Fast path: `length(borrowed)` wanted as a raw Int. The header
        // count lives at obj+4; reading it inline avoids the two calls
        // (`rt_obj_addr` + `rt_make_int`) and the box-then-unbox the
        // generic path pays — hot in `while i < length(arr)` conditions.
        // Restricted to a borrowed arg so we don't skip the owned-arg
        // temp release the boxed `compile_bare_length` path performs.
        if want == ValueShape::RawInt {
            if let Expression::CallExpression(ce) = expr {
                if self.is_bare_length_call(ce) {
                    let arg = &ce.args[0].value;
                    if !self.expr_transfers_ownership(arg) {
                        self.compile_expr_as(arg, ValueShape::Boxed)?;
                        // inline rt_obj_addr: mask NaN-box tag bits, wrap to i32
                        self.emit(Instruction::I64Const(0x0000_FFFF_FFFF_FFFF));
                        self.emit(Instruction::I64And);
                        self.emit(Instruction::I32WrapI64);
                        self.emit(Instruction::I32Load(mem_off(4)));
                        self.emit(Instruction::I64ExtendI32S);
                        return Ok(());
                    }
                }
            }
        }
        // Fast paths: when a caller wants a raw shape, skip the
        // box-then-unbox round-trip compile_expr would do. compile_expr
        // defaults to Boxed (so the many call sites that discard the
        // returned shape still see a valid NaN-boxed value), and this
        // function carves out the raw paths where we can do better.
        match (expr, want) {
            (Expression::NumberExpression(n), ValueShape::RawInt) => {
                // Both Int literals (`0`) and Float literals assigned
                // into a declared-Int slot (`let x Int = 3.7`) land
                // here. For the float case the `as i64` conversion
                // truncates toward zero — same semantics as the
                // RawFloat→RawInt runtime path (`I32TruncF64S`) and
                // what the user's "let myInt Int = 0.0 should work"
                // example implies.
                self.emit(Instruction::I64Const(n.value as i64));
                return Ok(());
            }
            (Expression::NumberExpression(n), ValueShape::RawFloat) => {
                self.emit(Instruction::F64Const(n.value));
                return Ok(());
            }
            (Expression::BooleanExpression(b), ValueShape::RawBool) => {
                self.emit(Instruction::I32Const(if b.value { 1 } else { 0 }));
                return Ok(());
            }
            (Expression::IdentifierExpression(id), _) => {
                if let Some(Resolve::Local(local)) = self.resolve(&id.name) {
                    if local.is_cell {
                        // Cell-bound: local holds an i32 cell address;
                        // dereference the value slot (@8, plan 114) to get
                        // the Boxed value, then convert.
                        self.emit(Instruction::LocalGet(local.local));
                        self.emit(Instruction::I64Load(mem_off(8)));
                        return self.emit_convert(ValueShape::Boxed, want);
                    }
                    self.emit(Instruction::LocalGet(local.local));
                    return self.emit_convert(local.shape, want);
                }
            }
            _ => {}
        }
        let got = self.compile_expr(expr)?;
        self.emit_convert(got, want)
    }

    fn expr_result_for_compiled(&mut self, expr: &Expression, shape: ValueShape) -> ExprResult {
        match shape {
            ValueShape::Boxed => {
                // Scalar fast path: a value the checker (or its storage
                // shape) proves is an Int/Float/Bool is a NaN-boxed
                // scalar, never a heap object. `rt_is_obj` is always
                // false for it, so a `retain` on a borrowed scalar is a
                // guaranteed no-op. Classify it Primitive so the
                // borrowed-return / borrowed-arg paths skip the
                // pointless `call $rt_retain` (and its paired ownership
                // event) entirely. Owned scalars stay Owned — they emit
                // no retain already, and leaving them untouched keeps
                // the store/transfer paths byte-for-byte unchanged.
                let owned = self.expr_transfers_ownership(expr);
                if !owned && self.expr_is_scalar_value(expr) {
                    ExprResult::primitive(ValueShape::Boxed)
                } else {
                    ExprResult::boxed(owned)
                }
            }
            _ => ExprResult::primitive(shape),
        }
    }

    /// True when `expr` provably evaluates to a scalar (Int / Float /
    /// Bool) — a NaN-boxed value that is never a heap object. Reference
    /// counting (`retain`/`release`) on such a value is a guaranteed
    /// no-op, so the boxed result can be classified `Primitive` and the
    /// RC helper call elided. Conservative by construction: every signal
    /// below is sound, and an unknown type falls through to `false`
    /// (keeping the existing retain), so this can only remove provably
    /// dead RC work.
    fn expr_is_scalar_value(&self, expr: &Expression) -> bool {
        // The checker's static type is the most general signal.
        if matches!(
            self.shape_for_expr(expr),
            ValueShape::RawInt | ValueShape::RawFloat | ValueShape::RawBool
        ) {
            return true;
        }
        // Literals are always scalars.
        if matches!(
            expr,
            Expression::NumberExpression(_) | Expression::BooleanExpression(_)
        ) {
            return true;
        }
        // A binding stored in a raw-scalar local holds a scalar
        // regardless of what the checker type map recorded for the
        // reference site (e.g. closure-local reads).
        if let Expression::IdentifierExpression(id) = expr {
            if let Some(binding) = self.lookup(&id.name) {
                if matches!(
                    binding.shape,
                    ValueShape::RawInt | ValueShape::RawFloat | ValueShape::RawBool
                ) {
                    return true;
                }
            }
        }
        false
    }

    fn compile_expr_result(&mut self, expr: &Expression) -> Result<ExprResult, BuildError> {
        let shape = self.compile_expr(expr)?;
        Ok(self.expr_result_for_compiled(expr, shape))
    }

    pub(super) fn compile_expr_result_as(
        &mut self,
        expr: &Expression,
        want: ValueShape,
    ) -> Result<ExprResult, BuildError> {
        self.compile_expr_as(expr, want)?;
        Ok(self.expr_result_for_compiled(expr, want))
    }

    pub(super) fn compile_numeric_expr_as_float(&mut self, expr: &Expression) -> Result<(), BuildError> {
        match self.numeric_shape_for_expr(expr) {
            Some(ValueShape::RawInt) => {
                self.compile_expr_as(expr, ValueShape::RawInt)?;
                self.emit_convert(ValueShape::RawInt, ValueShape::RawFloat)
            }
            Some(ValueShape::RawFloat) => self.compile_expr_as(expr, ValueShape::RawFloat),
            _ => self.compile_expr_as(expr, ValueShape::RawFloat),
        }
    }

    pub(super) fn emit_convert(&mut self, from: ValueShape, to: ValueShape) -> Result<(), BuildError> {
        match (from, to) {
            (a, b) if a == b => {}
            (ValueShape::RawInt, ValueShape::Boxed) => {
                self.emit(Instruction::I32WrapI64);
                self.emit(Instruction::I64ExtendI32U);
                self.emit(Instruction::I64Const(QNAN | TAG_INT));
                self.emit(Instruction::I64Or);
            }
            (ValueShape::Boxed, ValueShape::RawInt) => {
                self.emit(Instruction::I32WrapI64);
                self.emit(Instruction::I64ExtendI32S);
            }
            (ValueShape::RawFloat, ValueShape::Boxed) => {
                self.emit(Instruction::I64ReinterpretF64);
            }
            (ValueShape::Boxed, ValueShape::RawFloat) => {
                self.emit(Instruction::F64ReinterpretI64);
            }
            (ValueShape::RawBool, ValueShape::Boxed) => {
                self.emit(Instruction::I64ExtendI32U);
                self.emit(Instruction::I64Const(QNAN | TAG_BOOL));
                self.emit(Instruction::I64Or);
            }
            (ValueShape::Boxed, ValueShape::RawBool) => {
                self.emit(Instruction::I32WrapI64);
                self.emit(Instruction::I32Const(1));
                self.emit(Instruction::I32And);
            }
            (ValueShape::RawInt, ValueShape::RawFloat) => {
                self.emit(Instruction::F64ConvertI64S);
            }
            (ValueShape::RawFloat, ValueShape::RawInt) => {
                // Narrow f64 → i32 (forai Ints are i32-sized payloads)
                // then widen back to the i64 the local storage uses.
                self.emit(Instruction::I32TruncF64S);
                self.emit(Instruction::I64ExtendI32S);
            }
            _ => return Err(BuildError::UnsupportedExpression("shape-conversion")),
        }
        Ok(())
    }

    /// Open a structured control-flow label (`block` / `loop` / `if`)
    /// and keep `block_depth` in sync.
    pub(super) fn emit_open(&mut self, i: Instruction<'static>) {
        self.instrs.push(i);
        self.block_depth += 1;
    }

    /// Close a structured label (`End`). Panics on unbalanced opens —
    /// the builder is the only source of opens so this would be a
    /// bug, not a user error.
    pub(super) fn emit_close(&mut self) {
        self.instrs.push(Instruction::End);
        self.block_depth = self
            .block_depth
            .checked_sub(1)
            .expect("direct builder: unbalanced End");
    }

    pub(super) fn emit(&mut self, i: Instruction<'static>) {
        self.instrs.push(i);
    }

    pub(super) fn alloc_local(&mut self) -> u32 {
        self.alloc_typed_local(ValueShape::Boxed)
    }

    fn alloc_typed_local(&mut self, shape: ValueShape) -> u32 {
        let idx = self.next_local;
        self.next_local += 1;
        self.local_decls.push(match shape {
            ValueShape::Boxed | ValueShape::RawInt => ValType::I64,
            ValueShape::RawFloat => ValType::F64,
            ValueShape::RawBool => ValType::I32,
        });
        idx
    }

    pub(super) fn lookup(&self, name: &str) -> Option<LocalBinding> {
        for scope in self.scopes.iter().rev() {
            if let Some(&binding) = scope.get(name) {
                return Some(binding);
            }
        }
        None
    }

    /// Resolve an identifier: local in our own scope stack, an upvalue
    /// captured from the enclosing function's scope, or a module-level
    /// `var` global. Allocates a fresh upvalue slot on first reference
    /// to an outer name. Module vars are checked last so a local `var`
    /// or a captured upvalue of the same name takes precedence —
    /// ordinary lexical shadowing.
    pub(super) fn resolve(&mut self, name: &str) -> Option<Resolve> {
        if let Some(local) = self.lookup(name) {
            return Some(Resolve::Local(local));
        }
        if let Some(&i) = self.upvalue_by_name.get(name) {
            return Some(Resolve::Upvalue(i));
        }
        if let Some(outer) = self.outer_scope {
            if let Some(capture) = outer.lookup(name) {
                let uv_idx = self.upvalues.len() as u32;
                self.upvalues.push(capture);
                self.upvalue_by_name.insert(name.to_string(), uv_idx);
                return Some(Resolve::Upvalue(uv_idx));
            }
        }
        if let Some(&idx) = self.ctx.module_vars.get(name) {
            return Some(Resolve::ModuleVar(idx));
        }
        None
    }

    /// Emit the instructions that read upvalue `i` onto the stack:
    /// `env_ptr + i*8 -> I64Load`. Valid only inside a closure body.
    /// Cell-bound upvalues require one extra dereference: the env slot
    /// holds the NaN-boxed cell (plan 114), so unbox the address and
    /// `i64.load` the value slot at offset 8.
    pub(super) fn emit_upvalue_read(&mut self, uv_idx: u32) {
        self.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
        self.emit(Instruction::I64Load(mem_off(uv_idx as u64 * 8)));
        if self.upvalues[uv_idx as usize].is_cell {
            self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
            self.emit(Instruction::I64Load(mem_off(8)));
        }
    }

    pub(super) fn bind(&mut self, name: &str, local: u32) {
        self.bind_shape(name, local, ValueShape::Boxed);
    }

    fn bind_shape(&mut self, name: &str, local: u32, shape: ValueShape) {
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            LocalBinding {
                local,
                shape,
                is_cell: false,
            },
        );
    }

    /// Bind `name` to a cell-backed slot. `addr_local` is an i32 local
    /// holding the cell's heap address (the logical pointer of a tagged
    /// `OBJ_TAG_CELL` block since plan 114); reads/writes on the name
    /// dereference the cell's value slot at offset 8. The stored value
    /// is always `Boxed`.
    pub(super) fn bind_cell(&mut self, name: &str, addr_local: u32) {
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            LocalBinding {
                local: addr_local,
                shape: ValueShape::Boxed,
                is_cell: true,
            },
        );
    }

    pub(super) fn prepare_stack_for_owning_store(&mut self, result: ExprResult) {
        if result.shape != ValueShape::Boxed {
            return;
        }
        match result.ownership {
            ExprOwnership::Borrowed => {
                self.emit_ownership_event_for_stack(OwnershipOp::Retain, OWNERSHIP_SITE_UNKNOWN, 0);
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            }
            ExprOwnership::Owned => {
                self.emit_ownership_event_for_stack(
                    OwnershipOp::Transfer,
                    OWNERSHIP_SITE_UNKNOWN,
                    0,
                );
            }
            ExprOwnership::Primitive => {}
        }
    }

    /// Store the Boxed value currently on the stack into the cell whose
    /// address is in `addr_local`, with value-RC (plan 114): the cell OWNS
    /// its value, so retain a borrowed source, release the previous value,
    /// then write the slot at offset 8. The previous value is released
    /// AFTER the new one is computed, so a self-referencing write
    /// (`s = s + x`) reads the old value safely.
    pub(super) fn emit_cell_store(&mut self, addr_local: u32, result: ExprResult) {
        self.prepare_stack_for_owning_store(result);
        let tmp = self.alloc_local();
        self.emit(Instruction::LocalSet(tmp));
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::I64Load(mem_off(8)));
        self.emit_ownership_event_for_stack(OwnershipOp::Release, OWNERSHIP_SITE_UNKNOWN, 0);
        self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Store(mem_off(8)));
        self.emit_ownership_event_for_local(OwnershipOp::Store, OWNERSHIP_SITE_UNKNOWN, tmp, 0);
    }

    /// Allocate a fresh tagged cell (`OBJ_TAG_CELL`, 16 bytes, rc=1 from
    /// the allocator) with a zeroed value slot, leaving its logical
    /// address in a new i32 local. The zero value makes the first
    /// `emit_cell_store`'s release-the-old a safe no-op (RT_ALLOC reuses
    /// free-list blocks without clearing them).
    fn emit_cell_alloc(&mut self) -> u32 {
        let addr_local = self.alloc_i32_local();
        self.emit(Instruction::I32Const(16));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalTee(addr_local));
        // tag@0 + zeroed pad@4 in one i64 store.
        self.emit(Instruction::I64Const(OBJ_TAG_CELL as i64));
        self.emit(Instruction::I64Store(mem0()));
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Store(mem_off(8)));
        addr_local
    }

    fn store_field_value(&mut self, result: ExprResult) {
        self.prepare_stack_for_owning_store(result);
        self.emit_ownership_event_for_stack(OwnershipOp::Store, OWNERSHIP_SITE_UNKNOWN, 0);
        self.emit(Instruction::Call(self.rt().base + RT_SET_FIELD));
    }

    fn store_index_slot(&mut self, slot: u32, result: ExprResult) {
        self.prepare_stack_for_owning_store(result);
        let newv = self.alloc_local();
        self.emit(Instruction::LocalSet(newv));
        self.emit(Instruction::LocalGet(slot));
        self.emit(Instruction::I64Load(mem0()));
        self.emit_ownership_event_for_stack(OwnershipOp::Release, OWNERSHIP_SITE_UNKNOWN, 0);
        self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        self.emit(Instruction::LocalGet(slot));
        self.emit(Instruction::LocalGet(newv));
        self.emit(Instruction::I64Store(mem0()));
        self.emit_ownership_event_for_local(OwnershipOp::Store, OWNERSHIP_SITE_UNKNOWN, newv, 0);
    }

    pub(super) fn assign_to_async_frame_slot(&mut self, local: u32, result: ExprResult, owns_slot: bool) {
        let aux = OwnershipAux::AsyncFrameSlot.encode(local as u16);
        if owns_slot {
            self.prepare_stack_for_owning_store(result);
            self.emit(Instruction::LocalGet(local));
            self.emit_ownership_event_for_stack(OwnershipOp::Release, OWNERSHIP_SITE_UNKNOWN, aux);
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            self.emit(Instruction::LocalSet(local));
            self.emit_ownership_event_for_local(
                OwnershipOp::Overwrite,
                OWNERSHIP_SITE_UNKNOWN,
                local,
                aux,
            );
        } else {
            self.emit(Instruction::LocalSet(local));
        }
    }

    pub(super) fn capture_into_closure(&mut self, upvalue_index: usize) {
        let aux = OwnershipAux::ClosureCapture.encode(upvalue_index as u16);
        self.emit_ownership_event_for_stack(OwnershipOp::Retain, OWNERSHIP_SITE_UNKNOWN, aux);
        self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
        self.emit_ownership_event_for_stack(OwnershipOp::Store, OWNERSHIP_SITE_UNKNOWN, aux);
    }

    fn bind_to_local(
        &mut self,
        name: &str,
        result: ExprResult,
        release_at_scope_exit: bool,
    ) -> u32 {
        if result.shape == ValueShape::Boxed {
            match result.ownership {
                ExprOwnership::Borrowed => {
                    self.emit_ownership_event_for_stack(
                        OwnershipOp::Retain,
                        OWNERSHIP_SITE_UNKNOWN,
                        0,
                    );
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                ExprOwnership::Owned => {
                    self.emit_ownership_event_for_stack(
                        OwnershipOp::Transfer,
                        OWNERSHIP_SITE_UNKNOWN,
                        0,
                    );
                }
                ExprOwnership::Primitive => {}
            }
        }
        let local = self.alloc_typed_local(result.shape);
        self.emit(Instruction::LocalSet(local));
        self.bind_shape(name, local, result.shape);
        if release_at_scope_exit && result.shape == ValueShape::Boxed {
            self.note_droppable(local);
            self.emit_ownership_event_for_local(
                OwnershipOp::Store,
                OWNERSHIP_SITE_UNKNOWN,
                local,
                0,
            );
        }
        local
    }

    fn assign_to_local_slot(&mut self, binding: LocalBinding, result: ExprResult) {
        if binding.shape == ValueShape::Boxed && self.is_owned_local(binding.local) {
            match result.ownership {
                ExprOwnership::Borrowed => {
                    self.emit_ownership_event_for_stack(
                        OwnershipOp::Retain,
                        OWNERSHIP_SITE_UNKNOWN,
                        0,
                    );
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                ExprOwnership::Owned => {
                    self.emit_ownership_event_for_stack(
                        OwnershipOp::Transfer,
                        OWNERSHIP_SITE_UNKNOWN,
                        0,
                    );
                }
                ExprOwnership::Primitive => {}
            }
            self.emit(Instruction::LocalGet(binding.local));
            self.emit_ownership_event_for_stack(OwnershipOp::Release, OWNERSHIP_SITE_UNKNOWN, 0);
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            self.emit(Instruction::LocalSet(binding.local));
            self.emit_ownership_event_for_local(
                OwnershipOp::Overwrite,
                OWNERSHIP_SITE_UNKNOWN,
                binding.local,
                0,
            );
        } else {
            self.emit(Instruction::LocalSet(binding.local));
        }
    }

    fn assign_to_global_slot(&mut self, global_idx: u32, result: ExprResult) {
        debug_assert_eq!(result.shape, ValueShape::Boxed);
        match result.ownership {
            ExprOwnership::Borrowed => {
                self.emit_ownership_event_for_stack(OwnershipOp::Retain, OWNERSHIP_SITE_UNKNOWN, 0);
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            }
            ExprOwnership::Owned => {
                self.emit_ownership_event_for_stack(
                    OwnershipOp::Transfer,
                    OWNERSHIP_SITE_UNKNOWN,
                    0,
                );
            }
            ExprOwnership::Primitive => {}
        }
        self.emit(Instruction::GlobalGet(global_idx));
        self.emit_ownership_event_for_stack(OwnershipOp::Release, OWNERSHIP_SITE_UNKNOWN, 0);
        self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        self.emit(Instruction::GlobalSet(global_idx));
    }

    pub(super) fn discard_value(&mut self, result: ExprResult) {
        if result.shape == ValueShape::Boxed && result.ownership == ExprOwnership::Owned {
            self.emit_ownership_event_for_stack(OwnershipOp::Discard, OWNERSHIP_SITE_UNKNOWN, 0);
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        } else {
            self.emit(Instruction::Drop);
        }
    }

    pub(super) fn release_owned_local(&mut self, local: u32, op: OwnershipOp) {
        self.emit(Instruction::LocalGet(local));
        self.emit_ownership_event_for_stack(op, OWNERSHIP_SITE_UNKNOWN, 0);
        self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
    }

    fn return_value(&mut self, value: Option<&Expression>) -> Result<(), BuildError> {
        match value {
            Some(expr) => {
                let result = self.compile_expr_result_as(expr, ValueShape::Boxed)?;
                if result.ownership == ExprOwnership::Borrowed {
                    self.emit_ownership_event_for_stack(
                        OwnershipOp::Retain,
                        OWNERSHIP_SITE_UNKNOWN,
                        0,
                    );
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                self.emit_ownership_event_for_stack(OwnershipOp::Return, OWNERSHIP_SITE_UNKNOWN, 0);
                if self.has_active_drops() {
                    let saved = self.alloc_local();
                    self.emit(Instruction::LocalSet(saved));
                    self.emit_all_active_drops();
                    self.emit(Instruction::LocalGet(saved));
                }
            }
            None => {
                self.emit_all_active_drops();
                self.emit(Instruction::I64Const(VAL_VOID));
            }
        }
        self.emit_debug_function_end();
        self.emit(Instruction::Return);
        Ok(())
    }

    pub(super) fn emit_typed_param_prelude(&mut self) -> Result<(), BuildError> {
        let typed_params: Vec<(u32, String, ValueShape)> = self
            .fd
            .params
            .iter()
            .enumerate()
            .filter_map(|(idx, param)| {
                let shape = shape_for_type_node(&param.type_node);
                (shape != ValueShape::Boxed).then(|| (idx as u32, param.name.clone(), shape))
            })
            .collect();

        for (param_idx, name, shape) in typed_params {
            let local = self.alloc_typed_local(shape);
            self.emit(Instruction::LocalGet(param_idx));
            self.emit_convert(ValueShape::Boxed, shape)?;
            self.emit(Instruction::LocalSet(local));
            self.bind_shape(&name, local, shape);
        }
        Ok(())
    }

    /// Compile the function body: every statement except the last is a
    /// side-effect; the last statement is the return value (`@return`).
    /// Empty bodies return Void. Matches the legacy compiler's
    /// "last statement is tail position" convention.
    pub(super) fn compile_body(&mut self) -> Result<(), BuildError> {
        self.emit_debug_function_start();
        // Spy/mock preamble: emit only for top-level functions that
        // were referenced by `mock()` / `assert.*` in a test block.
        // `function_by_name` is keyed by the fully-qualified name
        // used in the unified function table, which matches
        // `fd.name` after `build_program_full` prefixes module funcs.
        if let Some(&fn_id) = self.function_by_name.get(self.fd.name.as_str()) {
            if self.ctx.mocked_fn_ids.contains(&fn_id) {
                self.emit_spy_preamble(fn_id)?;
            }
        }
        self.emit_typed_param_prelude()?;
        if self.fd.body.is_empty() {
            self.emit(Instruction::I64Const(VAL_VOID));
            self.emit_debug_function_end();
            self.emit(Instruction::Return);
            return Ok(());
        }
        // `<__start__>` is the synthesised wrapper that calls
        // `<__module_init__>` then user `main`. Drain the deferred
        // event queue once everything has run so any
        // `events.emitDeferred` from main / module init / event
        // subscribers gets dispatched before the program exits.
        // The drain has to happen between the tail expression
        // evaluating and the `Return` so main's return value (which
        // hosts read off `_start`) survives. See Phase 5 of
        // plans/event-system.md.
        let is_start = self.fd.name == "<__start__>";
        // Phase 3 reclamation (plans/111): confined fresh-literal bindings are
        // freed at scope exit via the unified `scope_drops` mechanism —
        // `pop_scope` for fall-through, and `compile_tail_stmt`/`compile_return`
        // for returns. `compile_body` just drives the tail; the cleanup lives
        // there.
        let last = self.fd.body.len() - 1;
        for (i, stmt) in self.fd.body.iter().enumerate() {
            if i == last {
                if is_start {
                    if let Statement::ExpressionStatement(es) = stmt {
                        self.compile_expr_as(&es.expression, ValueShape::Boxed)?;
                        let saved = self.alloc_local();
                        self.emit(Instruction::LocalSet(saved));
                        self.emit_import_call(crate::runtime::IMPORT_EVENT_DRAIN);
                        self.emit(Instruction::LocalGet(saved));
                        self.emit_debug_function_end();
                        self.emit(Instruction::Return);
                    } else {
                        // `<__start__>` bodies are always built from
                        // mk_call_stmt() ExpressionStatements; this
                        // fall-through is just a safety net.
                        self.compile_tail_stmt(stmt)?;
                    }
                } else {
                    self.compile_tail_stmt(stmt)?;
                }
            } else {
                self.compile_stmt(stmt)?;
            }
        }
        Ok(())
    }

    /// Emit the call-interception preamble for `fn_id`:
    ///
    ///   args_ptr = alloc(N * 8)
    ///   for i in 0..N: args_ptr[i*8] = local[i]
    ///   out_ptr = alloc(8)
    ///   if spy_check_call(fn_id, args_ptr, N, out_ptr) != 0:
    ///     return *out_ptr
    ///   ; else fall through to the real body
    ///
    /// Param count comes from `fd.params` + any `type_params`
    /// (generic `@type T` locals), matching the arity that
    /// `FunctionInfo.param_count` records.
    fn emit_spy_preamble(&mut self, fn_id: u32) -> Result<(), BuildError> {
        let arity = self.fd.params.len() as u32 + self.fd.type_params.len() as u32;

        // Serialise params to a freshly-allocated buffer. `RT_ALLOC`
        // hands out 8-byte-aligned pointers so aligned i64 stores
        // are safe.
        let args_ptr = self.alloc_i32_local();
        self.emit(Instruction::I32Const((arity as i32).max(1) * 8));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(args_ptr));
        self.emit(Instruction::LocalGet(args_ptr));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Store(mem0()));
        for i in 0..arity {
            self.emit(Instruction::LocalGet(args_ptr));
            self.emit(Instruction::LocalGet(i));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        // Output slot for the mock return value.
        let out_ptr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(out_ptr));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Store(mem0()));

        // spy_check_call(fn_id, args_ptr, arity, out_ptr) -> i32
        self.emit(Instruction::I32Const(fn_id as i32));
        self.emit(Instruction::LocalGet(args_ptr));
        self.emit(Instruction::I32Const(arity as i32));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit_import_call(crate::runtime::IMPORT_SPY_CHECK_CALL);

        // If the import returned 1, load *out_ptr and return it.
        let mocked = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(mocked));
        self.emit(Instruction::LocalGet(mocked));
        self.emit_open(Instruction::If(BlockType::Empty));
        let mocked_value = self.alloc_local();
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I64Load(mem0()));
        self.emit(Instruction::LocalSet(mocked_value));
        self.emit(Instruction::LocalGet(args_ptr));
        self.emit(Instruction::I32Const((arity as i32).max(1) * 8));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(mocked_value));
        self.emit_debug_function_end();
        self.emit(Instruction::Return);
        self.emit_close();
        self.emit(Instruction::LocalGet(args_ptr));
        self.emit(Instruction::I32Const((arity as i32).max(1) * 8));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        Ok(())
    }

    pub(super) fn compile_stmt(&mut self, stmt: &Statement) -> Result<(), BuildError> {
        match stmt {
            Statement::LetStatement(s) => self.compile_let(s),
            Statement::VarStatement(s) => self.compile_var(s),
            Statement::AssignmentStatement(a) => self.compile_assignment(a),
            Statement::IfStatement(s) => self.compile_if(s),
            Statement::CaseStatement(s) => self.compile_case(s, false),
            Statement::WhileStatement(s) => self.compile_while(s),
            Statement::BreakStatement(_) => self.compile_break(),
            Statement::ContinueStatement(_) => self.compile_continue(),
            Statement::ReturnStatement(r) => self.compile_return(r),
            Statement::TryStatement(s) => self.compile_try(s),
            Statement::ThrowStatement(s) => self.compile_throw(s),
            Statement::ForStatement(s) => self.compile_for(s),
            Statement::NowaitStatement(n) => self.compile_nowait(n),
            Statement::ExpressionStatement(es) => {
                let result = self.compile_expr_result(&es.expression)?;
                self.discard_value(result);
                Ok(())
            }
            Statement::UseStatement(_) => {
                // `use` inside a function body is a no-op at emission
                // time — module resolution already ran during
                // `prepare_source` / the checker. Top-level `use`s are
                // filtered out before the per-function emit anyway.
                Ok(())
            }
            _ => Err(BuildError::UnsupportedStatement(stmt_variant_name(stmt))),
        }
    }

    /// Emit code that pops a NaN-boxed i64 value and pushes an i32:
    /// 1 if truthy, 0 if falsy. Matches VM `val_to_bool` — null, void,
    /// and `false` are falsy; everything else (including Int 0 and
    /// the empty string) is truthy.
    pub(super) fn emit_truthy_i32(&mut self) {
        // Stash the value so we can compare it against three sentinels.
        let tmp = self.alloc_local();
        self.emit(Instruction::LocalSet(tmp));
        // `true` iff NOT (val == VAL_NULL || val == VAL_VOID || val == FALSE).
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Const(VAL_VOID));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::I32Or);
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Const(QNAN | TAG_BOOL));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::I32Or);
        // Invert: 0 → truthy (1), non-0 → falsy (0).
        self.emit(Instruction::I32Eqz);
    }

    pub(super) fn compile_truthy_i32(&mut self, e: &Expression) -> Result<(), BuildError> {
        // Statically-Bool condition fast path: compile straight to a
        // raw i32 0/1 and branch on it — no box, no generic
        // VAL_NULL/VOID/false truthiness comparison. `compile_expr`'s
        // identifier arm force-boxes a `RawBool` local (so a bare
        // `while running` / `if flag` would otherwise box the bool and
        // run the full 3-sentinel truthiness check every iteration),
        // which this path sidesteps. Sound because the only runtime
        // representations of a Bool are a raw i32 0/1 or a NaN-boxed
        // Bool that carries its value in the low bit — and the
        // `Boxed→RawBool` conversion (`wrap; and 1`) extracts exactly
        // that bit, correct for either. Hottest in the mandelbrot inner
        // `while running` loop.
        if self.expr_is_raw_bool(e) {
            return self.compile_expr_as(e, ValueShape::RawBool);
        }
        match self.compile_expr(e)? {
            ValueShape::RawBool => Ok(()),
            shape => {
                self.emit_convert(shape, ValueShape::Boxed)?;
                self.emit_truthy_i32();
                Ok(())
            }
        }
    }

    /// True when `e` provably evaluates to a Bool — the checker typed
    /// it `Bool`, or it reads a binding stored in a `RawBool` local.
    /// Conservative: an unknown type returns false and keeps the
    /// generic truthiness path, so this only ever removes provably
    /// redundant boxing + sentinel comparisons.
    fn expr_is_raw_bool(&self, e: &Expression) -> bool {
        if matches!(self.shape_for_expr(e), ValueShape::RawBool) {
            return true;
        }
        if let Expression::IdentifierExpression(id) = e {
            if let Some(binding) = self.lookup(&id.name) {
                return matches!(binding.shape, ValueShape::RawBool);
            }
        }
        false
    }

    /// `if cond1 body1 else if cond2 body2 else else_body end` lowers
    /// to a nested chain of wasm `if`/`else` blocks. Each branch
    /// evaluates its condition to an i32 truth flag, then emits the
    /// body under `if` and the next branch under `else`.
    fn compile_if(&mut self, s: &IfStatement) -> Result<(), BuildError> {
        self.compile_if_branches(&s.branches, s.else_branch.as_deref())
    }

    fn compile_if_branches(
        &mut self,
        branches: &[fai_compiler::ast::IfBranch],
        else_branch: Option<&[Statement]>,
    ) -> Result<(), BuildError> {
        if branches.is_empty() {
            // Only an `else` to emit.
            if let Some(body) = else_branch {
                self.push_scope();
                for st in body {
                    self.compile_stmt(st)?;
                }
                self.pop_scope();
            }
            return Ok(());
        }
        let first = &branches[0];
        self.compile_truthy_i32(&first.condition)?;
        self.emit_open(Instruction::If(BlockType::Empty));
        self.push_scope();
        for st in &first.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();
        if branches.len() > 1 || else_branch.is_some() {
            self.emit(Instruction::Else);
            self.compile_if_branches(&branches[1..], else_branch)?;
        }
        self.emit_close();
        Ok(())
    }

    /// `case value when m1 body1 when m2 body2 else default end`.
    /// Lowers to a nested if/else chain where each condition is
    /// `value == match_expr`. The value is evaluated once and
    /// parked in a local so every branch compares against the
    /// same NaN-box. Uses `RT_EQ` for comparison — matches forai's
    /// `==` semantics including String, Array, Dict deep equality.
    ///
    /// `is_tail = true` wires the case as a tail expression: each
    /// branch body emits `Return` via `compile_stmts_as_tail`, and
    /// the caller adds a fall-through `VAL_VOID; Return` after.
    fn compile_case(
        &mut self,
        cs: &fai_compiler::ast::CaseStatement,
        is_tail: bool,
    ) -> Result<(), BuildError> {
        // When the scrutinee is statically a String, `when 'literal'`
        // arms can compare bytes against the data-section literal
        // (rt_str_eq) instead of allocating a String for each literal and
        // running the generic rt_eq — the case-dispatch analogue of the
        // `s == 'literal'` fast path. Hot in router-style dispatch.
        let value_is_string = matches!(
            self.expression_type_at(&cs.value),
            Some(fai_checker::types::Type::String)
        );
        self.compile_expr_as(&cs.value, ValueShape::Boxed)?;
        let val_local = self.alloc_local();
        self.emit(Instruction::LocalSet(val_local));
        self.compile_case_branches(
            val_local,
            value_is_string,
            &cs.when_branches,
            cs.default_branch.as_deref(),
            is_tail,
        )
    }

    fn compile_case_branches(
        &mut self,
        val_local: u32,
        value_is_string: bool,
        branches: &[fai_compiler::ast::CaseBranch],
        default: Option<&[Statement]>,
        is_tail: bool,
    ) -> Result<(), BuildError> {
        if branches.is_empty() {
            // No more `when` arms — run `else` (if any).
            if let Some(body) = default {
                self.push_scope();
                if is_tail {
                    self.compile_stmts_as_tail(body)?;
                } else {
                    for st in body {
                        self.compile_stmt(st)?;
                    }
                }
                self.pop_scope();
            } else if is_tail {
                // No branch matched and no default — tail-context
                // demands a return value. Push Void.
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit_debug_function_end();
                self.emit(Instruction::Return);
            }
            return Ok(());
        }
        let first = &branches[0];
        // value == match_expr → i32 truth flag.
        if let (true, Expression::StringExpression(lit)) = (value_is_string, &first.match_expr) {
            // String scrutinee vs string-literal arm: compare bytes
            // against the interned data-section literal via rt_str_eq —
            // no String alloc for the literal, no generic rt_eq. The
            // scrutinee local is only read (obj_addr), not consumed.
            let (off, len) = self.ctx.strings.borrow_mut().intern(&lit.value);
            self.emit(Instruction::LocalGet(val_local));
            self.emit(Instruction::I64Const(0x0000_FFFF_FFFF_FFFF));
            self.emit(Instruction::I64And);
            self.emit(Instruction::I32WrapI64);
            let addr = self.alloc_i32_local();
            self.emit(Instruction::LocalTee(addr));
            self.emit(Instruction::I32Const(8));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::LocalGet(addr));
            self.emit(Instruction::I32Load(mem_off(4)));
            self.emit(Instruction::I32Const(off as i32));
            self.emit(Instruction::I32Const(len as i32));
            self.emit(Instruction::Call(self.rt().base + RT_STR_EQ));
        } else {
            self.emit(Instruction::LocalGet(val_local));
            self.compile_expr_as(&first.match_expr, ValueShape::Boxed)?;
            self.emit(Instruction::Call(self.rt().base + RT_EQ));
            self.emit_truthy_i32();
        }
        self.emit_open(Instruction::If(BlockType::Empty));
        self.push_scope();
        if is_tail {
            self.compile_stmts_as_tail(&first.body)?;
        } else {
            for st in &first.body {
                self.compile_stmt(st)?;
            }
        }
        self.pop_scope();
        if branches.len() > 1 || default.is_some() {
            self.emit(Instruction::Else);
            self.compile_case_branches(
                val_local,
                value_is_string,
                &branches[1..],
                default,
                is_tail,
            )?;
        }
        self.emit_close();
        Ok(())
    }

    /// Lower `while cond ... end` to:
    ///
    /// ```text
    /// (block $break
    ///   (loop $continue
    ///     <cond>; br_if $break if !cond
    ///     <body>
    ///     br $continue
    ///   )
    /// )
    /// ```
    fn compile_while(&mut self, s: &WhileStatement) -> Result<(), BuildError> {
        let cleanup_depth = self.cleanup_depth();
        self.emit_open(Instruction::Block(BlockType::Empty));
        let break_abs = self.block_depth;
        self.emit_open(Instruction::Loop(BlockType::Empty));
        let continue_abs = self.block_depth;
        self.loops.push(LoopFrame {
            break_abs,
            continue_abs,
            cleanup_depth,
        });

        // Evaluate condition + branch out on falsy.
        self.compile_truthy_i32(&s.condition)?;
        self.emit(Instruction::I32Eqz);
        // `br 1` = exit the outer `block` (break). From inside the
        // loop body at open, block_depth = continue_abs; `br` depth
        // to break is `continue_abs - break_abs = 1`.
        self.emit(Instruction::BrIf(self.block_depth - break_abs));

        self.push_scope();
        for st in &s.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        // Back-edge to the loop start.
        self.emit(Instruction::Br(self.block_depth - continue_abs));

        self.loops.pop();
        self.emit_close(); // end loop
        self.emit_close(); // end block
        Ok(())
    }

    fn compile_break(&mut self) -> Result<(), BuildError> {
        let frame = *self
            .loops
            .last()
            .ok_or(BuildError::UnsupportedStatement("break outside loop"))?;
        let rel = self.block_depth - frame.break_abs;
        self.emit_cleanup_to_depth(frame.cleanup_depth);
        self.emit(Instruction::Br(rel));
        Ok(())
    }

    fn compile_continue(&mut self) -> Result<(), BuildError> {
        let frame = *self
            .loops
            .last()
            .ok_or(BuildError::UnsupportedStatement("continue outside loop"))?;
        let rel = self.block_depth - frame.continue_abs;
        self.emit_cleanup_to_depth(frame.cleanup_depth);
        self.emit(Instruction::Br(rel));
        Ok(())
    }

    /// `return` with no value returns `Void`; `return <expr>` pushes
    /// the (boxed) expression value and returns it. Wasm functions
    /// have a single i64 result (boxed forai Value) regardless of the
    /// declared fai return type, so this always emits an i64.
    fn compile_return(&mut self, s: &fai_compiler::ast::ReturnStatement) -> Result<(), BuildError> {
        self.return_value(s.value.as_ref())
    }

    /// Lower `try ... catch e ... end` to two nested wasm blocks:
    ///
    /// ```text
    /// (block $after_try        ;; break target on normal completion
    ///   (block $catch_handler  ;; `throw` branches here
    ///     <try_body>
    ///     br $after_try        ;; skip catch body on success
    ///   )                      ;; end $catch_handler
    ///   ;; caught: err_local holds the thrown value
    ///   <catch_body with catch_name bound to err_local>
    /// )                        ;; end $after_try
    /// ```
    ///
    /// A `throw` inside the try body sets `err_local` and `br`s to
    /// `$catch_handler`. `finally` runs after both success and
    /// catch paths — the basic case (finally after clean success
    /// or caught error) works. An uncaught throw inside the catch
    /// body propagates without running finally; that matches the
    /// bytecode compiler's behaviour and is acceptable under forai's
    /// current error-handling contract.
    fn compile_try(&mut self, s: &TryStatement) -> Result<(), BuildError> {
        let err_local = self.alloc_local();
        let cleanup_depth = self.cleanup_depth();
        self.emit_open(Instruction::Block(BlockType::Empty));
        self.emit_open(Instruction::Block(BlockType::Empty));
        let catch_abs = self.block_depth;
        self.tries.push(TryFrame {
            catch_abs,
            cleanup_depth,
            err_local,
        });

        self.push_scope();
        for st in &s.try_body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        // Success path: skip the catch handler by branching to
        // `$after_try` (outer block).
        let after_try_rel = self.block_depth - (catch_abs - 1);
        self.emit(Instruction::Br(after_try_rel));

        // Done with the try body — pop the frame BEFORE catch_body
        // compiles so a `throw` inside catch targets the next-outer
        // try (or traps if none).
        self.tries.pop();

        self.emit_close(); // end $catch_handler

        // Catch handler: err_local holds the thrown value. Bind it
        // under the user-declared catch_name for the catch body.
        self.push_scope();
        self.bind(&s.catch_name, err_local);
        self.note_droppable(err_local);
        for st in &s.catch_body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        self.emit_close(); // end $after_try

        // `finally` — run after both success and catch paths. The
        // bytecode compiler emits the finally body here too (no
        // guaranteed-execution plumbing — a `throw` inside catch
        // propagates without running finally). The direct path
        // matches that behaviour for parity.
        if let Some(finally_body) = &s.finally_body {
            self.push_scope();
            for st in finally_body {
                self.compile_stmt(st)?;
            }
            self.pop_scope();
        }
        Ok(())
    }

    /// `try`/`catch` in tail position: the tail expression of the try body
    /// (or, on a caught throw, of the catch body) is the function's return
    /// value. The statement form (`compile_try`) discards that value, which
    /// would silently return Void — decoding as an empty/garbage heap value at
    /// the call site. A tail try must preserve it.
    ///
    /// Without a `finally`, each body is compiled as a tail and emits its own
    /// `Return` — exactly the shape `compile_if_branches_tail` uses. A `throw`
    /// inside the try body still `br`s to `$catch_handler`; the tail `Return`
    /// only fires on the success path.
    fn compile_try_as_tail(&mut self, s: &TryStatement) -> Result<(), BuildError> {
        if s.finally_body.is_some() {
            // The checker types a `finally` try as Void (it is not a value
            // expression), so a `finally` try only reaches tail position in a
            // Void-returning function. Preserve the statement lowering and
            // return Void — the same path the pre-tail `_` arm took.
            self.compile_try(s)?;
            self.emit_all_active_drops();
            self.emit(Instruction::I64Const(VAL_VOID));
            self.emit_debug_function_end();
            self.emit(Instruction::Return);
            return Ok(());
        }
        let err_local = self.alloc_local();
        let cleanup_depth = self.cleanup_depth();
        self.emit_open(Instruction::Block(BlockType::Empty)); // $after_try
        self.emit_open(Instruction::Block(BlockType::Empty)); // $catch_handler
        let catch_abs = self.block_depth;
        self.tries.push(TryFrame {
            catch_abs,
            cleanup_depth,
            err_local,
        });

        // Success path: the try body's tail expression returns.
        self.push_scope();
        self.compile_stmts_as_tail(&s.try_body)?;
        self.pop_scope();
        // Every path through `compile_stmts_as_tail` ends in `Return`, so no
        // `br $after_try` is needed — the catch handler is reached only via a
        // `throw`'s `br $catch_handler`.
        self.tries.pop();
        self.emit_close(); // end $catch_handler

        // Caught path: `err_local` holds the thrown value, bound under the
        // catch name; the catch body's tail expression returns.
        self.push_scope();
        self.bind(&s.catch_name, err_local);
        self.note_droppable(err_local);
        self.compile_stmts_as_tail(&s.catch_body)?;
        self.pop_scope();
        self.emit_close(); // end $after_try

        // Fall-through safety: both bodies already `Return`, so this trailer is
        // unreachable, but it keeps the function body stack-valid at its end —
        // the same trailer the tail `if`/`case` arms emit.
        self.emit(Instruction::I64Const(VAL_VOID));
        self.emit_debug_function_end();
        self.emit(Instruction::Return);
        Ok(())
    }

    /// Lower `throw expr`. Inside a `try`, stores the value into the
    /// innermost try's `err_local` and branches to `$catch_handler`
    /// — the inline fast path with no globals touched.
    ///
    /// Outside any try, stash the thrown value into the
    /// `error_flag`/`error_value` globals and return early with a
    /// placeholder result. The caller's post-call propagation check
    /// (see `emit_post_call_propagation`) will see the flag set and
    /// either deliver the error to its enclosing `try` or propagate
    /// further up. This is the unwind path that makes
    /// cross-function throw + catch work.
    /// Ensure the boxed value in `thrown_local` is safe to catch: a caught
    /// `e` is consumed through `e.message` (a dict field read), so throwing
    /// anything that is not a dict — a bare string, an Int, null — used to
    /// hand the catch site a value whose bytes were then dereferenced as
    /// dict entries (the ISSUES.md wasm-OOB `e.message` corruption). Any
    /// non-dict thrown value is replaced with `{message: toString(value)}`,
    /// the same shape `Error(msg)` builds; dicts (Error values and thrown
    /// records — records are dicts at runtime) pass through untouched.
    ///
    /// `thrown_owned` says whether `thrown_local` holds a +1 the throw path
    /// owns. When owned, the original's +1 is consumed into the wrapper
    /// (with the `RT_VALUE_TO_STR` string-alias case given its own retained
    /// ref first, mirroring `emit_to_string_owned`). When borrowed, the
    /// original is left untouched and only the message ref is retained.
    pub(super) fn emit_wrap_bare_throw(&mut self, thrown_local: u32, thrown_owned: bool) {
        let needs_wrap = self.alloc_i32_local();
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::LocalSet(needs_wrap));
        self.emit(Instruction::LocalGet(thrown_local));
        self.emit(Instruction::Call(self.rt().base + RT_IS_OBJ));
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::LocalGet(thrown_local));
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        self.emit(Instruction::I32Load(mem0()));
        self.emit(Instruction::I32Const(OBJ_TAG_DICT));
        self.emit(Instruction::I32Eq);
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(needs_wrap));
        self.emit_close();
        self.emit_close();

        self.emit(Instruction::LocalGet(needs_wrap));
        self.emit_open(Instruction::If(BlockType::Empty));
        {
            // msg = toString(thrown), uniformly owned: value_to_str is the
            // identity on a String, so the alias case retains.
            let msg_local = self.alloc_local();
            self.emit(Instruction::LocalGet(thrown_local));
            self.emit(Instruction::Call(self.rt().base + RT_VALUE_TO_STR));
            self.emit(Instruction::LocalSet(msg_local));
            self.emit(Instruction::LocalGet(msg_local));
            self.emit(Instruction::LocalGet(thrown_local));
            self.emit(Instruction::I64Eq);
            self.emit_open(Instruction::If(BlockType::Empty));
            self.emit(Instruction::LocalGet(msg_local));
            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            self.emit(Instruction::Drop);
            self.emit_close();
            if thrown_owned {
                // Consume the original's +1 (no-op on primitives; for the
                // string-alias case the msg ref above keeps it alive).
                // Discard event mirrors `release_stash` so the checker's
                // ledger for the original value balances.
                self.emit_ownership_event_for_local(
                    OwnershipOp::Discard,
                    OWNERSHIP_SITE_UNKNOWN,
                    thrown_local,
                    0,
                );
                self.emit(Instruction::LocalGet(thrown_local));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            }

            // {message: msg} — same 24-byte layout as `Error(msg)`
            // (see compile_error_construct).
            let (key_off, key_len) = self.ctx.strings.borrow_mut().intern("message");
            let dict_addr = self.alloc_i32_local();
            self.emit(Instruction::I32Const(24));
            self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
            self.emit(Instruction::LocalSet(dict_addr));
            self.emit(Instruction::LocalGet(dict_addr));
            self.emit(Instruction::I32Const(OBJ_TAG_DICT));
            self.emit(Instruction::I32Store(mem0()));
            self.emit(Instruction::LocalGet(dict_addr));
            self.emit(Instruction::I32Const(1));
            self.emit(Instruction::I32Store(mem_off(4)));
            self.emit(Instruction::LocalGet(dict_addr));
            self.emit(Instruction::I32Const(key_off as i32));
            self.emit(Instruction::I32Const(key_len as i32));
            self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
            self.emit(Instruction::I64Store(mem_off(8)));
            self.emit(Instruction::LocalGet(dict_addr));
            self.emit(Instruction::LocalGet(msg_local));
            self.emit(Instruction::I64Store(mem_off(16)));
            self.emit(Instruction::LocalGet(dict_addr));
            self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
            // Transfer event: the wrapper dict is the owned thrown value
            // from here on (mirrors compile_throw's Owned handling), so
            // the catch scope's eventual Cleanup of it matches.
            self.emit_ownership_event_for_stack(OwnershipOp::Transfer, OWNERSHIP_SITE_UNKNOWN, 0);
            self.emit(Instruction::LocalSet(thrown_local));
        }
        self.emit_close();
    }

    fn compile_throw(&mut self, s: &ThrowStatement) -> Result<(), BuildError> {
        let result = self.compile_expr_result_as(&s.expression, ValueShape::Boxed)?;
        match result.ownership {
            ExprOwnership::Borrowed => {
                self.emit_ownership_event_for_stack(OwnershipOp::Retain, OWNERSHIP_SITE_UNKNOWN, 0);
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            }
            ExprOwnership::Owned => {
                self.emit_ownership_event_for_stack(
                    OwnershipOp::Transfer,
                    OWNERSHIP_SITE_UNKNOWN,
                    0,
                );
            }
            ExprOwnership::Primitive => {}
        }
        let thrown_local = self.alloc_local();
        self.emit(Instruction::LocalSet(thrown_local));
        self.emit_throw_owned(thrown_local);
        Ok(())
    }

    /// True for frames where an *uncaught* error must terminate rather than
    /// return-with-flag: the program entry (`<__start__>`) and each test-case /
    /// hook wrapper (`<test:...>`, `<test-before-all:...>`,
    /// `<test-after-all:...>`). A test wrapper is invoked by the bare
    /// `_fai_run_test` dispatcher, which drops the wrapper's result and does
    /// NOT inspect `__error_flag` — so an uncaught error that merely set the
    /// flag would be read by the host as a PASS. Trapping (with a reported
    /// message) makes `run_case` return `Err`, which the host records as the
    /// failure. This also makes an uncaught `throw` in a test body a real
    /// failure, not just a failed `assert`.
    pub(super) fn frame_is_fatal_on_uncaught(&self) -> bool {
        // `<__start__>` (real entry), `main` (the entry itself — real builds
        // wrap it in `<__start__>`, but a standalone/single-fn build exports
        // `main` directly as `_start`), and each test-case / hook wrapper.
        self.fd.name == "<__start__>"
            || self.fd.name == "main"
            || self.fd.name.starts_with("<test")
    }

    /// Raise the owned `+1` value in `thrown_local` through the error channel:
    /// wrap a non-dict into `{message: ...}`, then either branch to the nearest
    /// enclosing `try`'s catch handler or (uncaught) stash it into the error
    /// globals and return — trapping instead in a fatal frame. Shared by
    /// `throw` and by a failed `assert` (which raises its message this way so
    /// `finally`/cleanup runs on the way out instead of being skipped by a hard
    /// trap).
    pub(super) fn emit_throw_owned(&mut self, thrown_local: u32) {
        self.emit_wrap_bare_throw(thrown_local, true);
        if let Some(frame) = self.tries.last().copied() {
            let rel = self.block_depth - frame.catch_abs;
            self.emit_cleanup_to_depth(frame.cleanup_depth);
            let err_local = frame.err_local;
            self.emit(Instruction::LocalGet(thrown_local));
            self.emit(Instruction::LocalSet(err_local));
            self.emit(Instruction::Br(rel));
        } else {
            self.emit_cleanup_to_depth(0);
            self.emit(Instruction::LocalGet(thrown_local));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_VALUE));
            if self.frame_is_fatal_on_uncaught() {
                // Report the error value so the trap names it, then trap —
                // the host reads it out and records the case failure.
                self.emit(Instruction::I32Const(crate::runtime::TRAP_UNCAUGHT_ERROR));
                self.emit(Instruction::GlobalGet(GLOBAL_ERROR_VALUE));
                self.emit(Instruction::I64Const(0));
                self.emit_import_call(IMPORT_TRAP_REPORT);
                self.emit(Instruction::Unreachable);
            } else {
                self.emit(Instruction::I32Const(1));
                self.emit(Instruction::GlobalSet(GLOBAL_ERROR_FLAG));
                // Placeholder return value — the caller throws it away
                // as soon as it sees the flag set.
                self.emit(Instruction::I64Const(0));
                self.emit_debug_function_end();
                self.emit(Instruction::Return);
            }
        }
    }

    /// Emit the post-call propagation check. The call's i64 result
    /// must already be on the stack. The check stashes the result
    /// into a local so the inner `If` block doesn't need to reach
    /// across wasm's per-block operand stack — if it then sees
    /// `error_flag` set, it either delivers the error to the
    /// enclosing `try` (Br to the catch handler) or returns early
    /// with the result still acting as the function's placeholder
    /// return value. If the flag isn't set, the saved result is
    /// pushed back so the caller's expression context sees an i64
    /// exactly as if no check had run.
    pub(super) fn release_owned_arg_stashes(&mut self, owned_arg_stashes: &[u32]) {
        for &t in owned_arg_stashes {
            self.emit_ownership_event_for_local(OwnershipOp::Discard, OWNERSHIP_SITE_UNKNOWN, t, 0);
            self.emit(Instruction::LocalGet(t));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
    }

    pub(super) fn emit_post_call_propagation(&mut self, owned_arg_stashes: &[u32]) {
        let result_local = self.alloc_local();
        self.emit(Instruction::LocalSet(result_local));
        self.emit(Instruction::GlobalGet(GLOBAL_ERROR_FLAG));
        self.emit_open(Instruction::If(BlockType::Empty));
        if let Some(frame) = self.tries.last().copied() {
            let rel = self.block_depth - frame.catch_abs;
            let err_local = frame.err_local;
            self.emit(Instruction::GlobalGet(GLOBAL_ERROR_VALUE));
            self.emit(Instruction::LocalSet(err_local));
            self.emit(Instruction::I32Const(0));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_FLAG));
            self.emit(Instruction::I64Const(0));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_VALUE));
            self.release_owned_arg_stashes(owned_arg_stashes);
            self.emit_cleanup_to_depth(frame.cleanup_depth);
            self.emit(Instruction::Br(rel));
        } else if let Some(async_ctx) = self.async_error_ctx {
            self.emit(Instruction::GlobalGet(GLOBAL_ERROR_VALUE));
            let err_local = self.alloc_local();
            self.emit(Instruction::LocalSet(err_local));
            self.emit(Instruction::I32Const(0));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_FLAG));
            self.emit(Instruction::I64Const(0));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_VALUE));
            self.release_owned_arg_stashes(owned_arg_stashes);
            match async_ctx.catch {
                Some((catch_blk, catch_local)) => {
                    self.emit(Instruction::LocalGet(err_local));
                    self.emit(Instruction::LocalSet(catch_local));
                    emit_store_current_rstate(self, &async_ctx.layout, catch_blk as i32);
                    self.emit(Instruction::Br(async_ctx.loop_depth + 1));
                }
                None => {
                    self.emit(Instruction::GlobalGet(async_ctx.layout.g_current));
                    self.emit(Instruction::LocalGet(err_local));
                    self.emit(Instruction::Call(async_ctx.layout.fail));
                    self.emit_debug_function_end();
                    self.emit(Instruction::Return);
                }
            }
        } else if self.frame_is_fatal_on_uncaught() {
            // Outermost frame (program entry) or a test-case wrapper — there is
            // nowhere left to propagate. A clean `Return` here would leave
            // `error_flag` set: at `<__start__>` that silently exits the
            // program (forai#4); in a test wrapper the bare `_fai_run_test`
            // dispatcher would read it as a PASS. Trap instead so the host
            // records the failure. Report the error value first so the trap
            // names it (plan 116).
            self.emit(Instruction::I32Const(crate::runtime::TRAP_UNCAUGHT_ERROR));
            self.emit(Instruction::GlobalGet(GLOBAL_ERROR_VALUE));
            self.emit(Instruction::I64Const(0));
            self.emit_import_call(IMPORT_TRAP_REPORT);
            self.emit(Instruction::Unreachable);
        } else {
            // Push the saved result and return — it's a placeholder
            // the caller throws away once it sees the flag set.
            self.release_owned_arg_stashes(owned_arg_stashes);
            self.emit_cleanup_to_depth(0);
            self.emit(Instruction::LocalGet(result_local));
            self.emit_debug_function_end();
            self.emit(Instruction::Return);
        }
        self.emit_close();
        self.emit(Instruction::LocalGet(result_local));
    }

    /// `nowait expr` — fire-and-forget: wrap `expr` in a zero-arg
    /// closure and hand it to `IMPORT_SPAWN`. The host dispatches
    /// the closure via `__indirect_function_table` (synchronous
    /// under the current tier-1 runtime — the scheduling boundary
    /// is the host's concern, not the guest's).
    ///
    /// Mirrors the bytecode compiler's `compile_implicit_closure` +
    /// `Op::Spawn` pattern. Shares the existing
    /// `compile_function_expression` closure-allocation path, so
    /// upvalue capture works automatically — a `nowait` inside a
    /// function that references outer locals wires them as closure
    /// upvalues without any special-casing here.
    fn compile_nowait(&mut self, s: &fai_compiler::ast::NowaitStatement) -> Result<(), BuildError> {
        // Synthesise a zero-arg FunctionDeclaration whose body is
        // the nowait expression (as a statement). `compile_function_expression`
        // treats it like any anonymous `do ... end`, emitting heap
        // allocation + upvalue capture and leaving the boxed closure
        // on the stack.
        let wrapper = fai_compiler::ast::FunctionDeclaration {
            name: format!("<nowait@{}:{}>", s.location.line, s.location.column),
            type_params: Vec::new(),
            params: Vec::new(),
            return_types: Vec::new(),
            body: vec![fai_compiler::ast::Statement::ExpressionStatement(
                fai_compiler::ast::ExpressionStatement {
                    expression: s.expression.clone(),
                    location: s.location.clone(),
                },
            )],
            doc: None,
            is_private: None,
            is_abstract: false,
            is_remote: false,
            auth_policy: None,
            location: s.location.clone(),
            doc_comment: None,
        };
        self.compile_function_expression(&wrapper)?;
        self.emit_import_call(IMPORT_SPAWN);
        // IMPORT_SPAWN returns i64 (VAL_VOID) — discard.
        self.emit(Instruction::Drop);
        Ok(())
    }

    /// Phase D for-range: `for i in start..end` lowers to a counter
    /// loop with raw i32 state. The `item_name` is rebound each
    /// iteration by NaN-boxing the current counter.
    ///
    /// `for x in iterable`. Dispatches on the iterable shape:
    /// `RangeExpression` inlines an i32 counter loop (the cheap
    /// path), and any other expression is treated as an Array —
    /// the runtime reads the object's length at offset 4 and
    /// loads elements from `addr + 8 + i*8`. Matches the bytecode
    /// compiler's split between `Op::ForRange` and `Op::ForLoop`.
    ///
    /// Dict iteration isn't wired (neither codegen path handles it
    /// today); it surfaces as `ForStatement/unsupported-iterable`
    /// if the runtime tag turns out to be non-Array — but the
    /// builder can't know that at compile time, so trust the
    /// checker's type validation.
    fn compile_for(&mut self, s: &ForStatement) -> Result<(), BuildError> {
        if let Expression::RangeExpression(r) = &s.items {
            return self.compile_for_range(s, r);
        }
        self.compile_for_array(s)
    }

    /// Generic array iteration — evaluate the iterable into a local,
    /// read its length, and walk `0..length` emitting element loads.
    /// Each iteration rebinds `item_name` to `array[index]`.
    ///
    /// Structure (three nested labels — required so `continue`
    /// reaches the increment, not the loop header):
    /// ```text
    /// (block $break              ; break target
    ///   (loop $repeat             ; repeat target — Br here re-runs the check
    ///     (block $continue         ; continue target — fall-through hits increment
    ///       <length check; br_if $break>
    ///       <load item; bind>
    ///       <body>                 ; break → $break, continue → $continue
    ///     )                         ; end $continue — falls through to:
    ///     index++
    ///     br $repeat
    ///   )
    /// )
    /// ```
    fn compile_for_array(&mut self, s: &ForStatement) -> Result<(), BuildError> {
        // Evaluate the iterable — typically an ArrayExpression or a
        // variable holding an Array. Box stays on the stack so we
        // can unbox its address once.
        self.compile_expr(&s.items)?;
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        let arr_addr = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(arr_addr));

        // length = mem[arr_addr + 4]
        let length = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::LocalSet(length));

        // index = 0
        let index = self.alloc_i32_local();
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(index));

        // item slot (NaN-boxed i64) — rebound each iteration.
        let item_local = self.alloc_local();

        let cleanup_depth = self.cleanup_depth();
        self.emit_open(Instruction::Block(BlockType::Empty)); // $break
        let break_abs = self.block_depth;
        self.emit_open(Instruction::Loop(BlockType::Empty)); // $repeat
        let repeat_abs = self.block_depth;
        self.emit_open(Instruction::Block(BlockType::Empty)); // $continue
        let continue_abs = self.block_depth;
        self.loops.push(LoopFrame {
            break_abs,
            continue_abs,
            cleanup_depth,
        });

        // if index >= length: break
        self.emit(Instruction::LocalGet(index));
        self.emit(Instruction::LocalGet(length));
        self.emit(Instruction::I32GeS);
        self.emit(Instruction::BrIf(self.block_depth - break_abs));

        // item = i64 at arr_addr + 8 + index*8
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalGet(index));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Mul);
        self.emit(Instruction::I32Add);
        self.emit(Instruction::I64Load(mem0()));
        self.emit(Instruction::LocalSet(item_local));

        self.push_scope();
        self.bind(&s.item_name, item_local);
        for st in &s.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        self.loops.pop();
        self.emit_close(); // end $continue — falls through to increment

        // index++
        self.emit(Instruction::LocalGet(index));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(index));
        // br $repeat — back to the check.
        self.emit(Instruction::Br(self.block_depth - repeat_abs));

        self.emit_close(); // end $repeat
        self.emit_close(); // end $break
        Ok(())
    }

    /// `for i in start..end` (inclusive range). Same three-label
    /// structure as `compile_for_array` — `continue` exits the inner
    /// block so the counter increment runs before looping.
    fn compile_for_range(
        &mut self,
        s: &ForStatement,
        range: &fai_compiler::ast::RangeExpression,
    ) -> Result<(), BuildError> {
        // Evaluate start and end, unbox to i32.
        self.compile_expr(&range.start)?;
        let start_i32 = self.alloc_i32_local();
        self.emit(Instruction::I32WrapI64);
        self.emit(Instruction::LocalSet(start_i32));
        self.compile_expr(&range.end)?;
        let end_i32 = self.alloc_i32_local();
        self.emit(Instruction::I32WrapI64);
        self.emit(Instruction::LocalSet(end_i32));
        // counter = start
        let counter = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(start_i32));
        self.emit(Instruction::LocalSet(counter));
        // The loop variable holds an Int (the range is integer-typed),
        // so bind it in a RawInt local rather than eagerly boxing the
        // counter with an `rt_make_int` CALL every iteration. Boxing now
        // happens lazily, only where the body actually needs a boxed
        // value — and uses like `arr[i]` / `total + i` stay fully native
        // (no box→unbox round trip). The shape-aware read/convert and
        // closure-capture machinery handles the rest.
        let item_local = self.alloc_typed_local(ValueShape::RawInt);

        let cleanup_depth = self.cleanup_depth();
        self.emit_open(Instruction::Block(BlockType::Empty)); // $break
        let break_abs = self.block_depth;
        self.emit_open(Instruction::Loop(BlockType::Empty)); // $repeat
        let repeat_abs = self.block_depth;
        self.emit_open(Instruction::Block(BlockType::Empty)); // $continue
        let continue_abs = self.block_depth;
        self.loops.push(LoopFrame {
            break_abs,
            continue_abs,
            cleanup_depth,
        });

        // `..` is exclusive (exit when counter >= end); `...` is
        // inclusive (exit when counter > end).
        self.emit(Instruction::LocalGet(counter));
        self.emit(Instruction::LocalGet(end_i32));
        if range.inclusive {
            self.emit(Instruction::I32GtS);
        } else {
            self.emit(Instruction::I32GeS);
        }
        self.emit(Instruction::BrIf(self.block_depth - break_abs));

        // item = counter (raw — no rt_make_int box)
        self.emit(Instruction::LocalGet(counter));
        self.emit(Instruction::I64ExtendI32S);
        self.emit(Instruction::LocalSet(item_local));

        self.push_scope();
        self.bind_shape(&s.item_name, item_local, ValueShape::RawInt);
        for st in &s.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        self.loops.pop();
        self.emit_close(); // end $continue

        // counter++
        self.emit(Instruction::LocalGet(counter));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(counter));
        self.emit(Instruction::Br(self.block_depth - repeat_abs));

        self.emit_close(); // end $repeat
        self.emit_close(); // end $break
        Ok(())
    }

    /// Allocate an i32-typed local. Mirrors `alloc_local` but for
    /// scratch counters that aren't NaN-boxed forai values.
    pub(super) fn alloc_i32_local(&mut self) -> u32 {
        let idx = self.next_local;
        self.next_local += 1;
        self.local_decls.push(ValType::I32);
        idx
    }

    fn compile_assignment(&mut self, a: &AssignmentStatement) -> Result<(), BuildError> {
        match &a.target {
            AssignmentTarget::Variables { names } if names.len() == 1 => {
                // Assignment dispatches on where the name resolves:
                //   • Own local, plain       → LocalSet.
                //   • Own local, cell-bound  → I64Store through the cell.
                //   • Upvalue referring to a cell → I64Store through the
                //     env-stored cell address. This is how closures
                //     mutate their enclosing scope's `var`s.
                //   • Upvalue referring to a snapshot → refused (the
                //     captured `let` isn't mutable).
                match self.resolve(&names[0]) {
                    Some(Resolve::Local(binding)) => {
                        if binding.is_cell {
                            // Cell-bound `var` shared with closures: the cell
                            // OWNS its value (plan 114) — retain-new-if-
                            // borrowed, release-old, store at offset 8. A
                            // sibling closure that kept the old value has its
                            // own retain, so the release can't free under it.
                            // (A kept old value also means rc > 1, so the
                            // append-move fast path declines and copies.)
                            let result =
                                match self.try_compile_move_form(&names[0], &a.value)? {
                                    Some(r) => r,
                                    None => self
                                        .compile_expr_result_as(&a.value, ValueShape::Boxed)?,
                                };
                            self.emit_cell_store(binding.local, result);
                        } else if binding.shape == ValueShape::Boxed
                            && self.is_owned_local(binding.local)
                        {
                            // Reassign an owned object local (RC, plan 113 R1):
                            // retain a borrowed new value (co-ownership), release
                            // the old value this slot owned, then store. The slot
                            // keeps owning exactly one ref.
                            let result =
                                match self.try_compile_move_form(&names[0], &a.value)? {
                                    Some(r) => r,
                                    None => self
                                        .compile_expr_result_as(&a.value, ValueShape::Boxed)?,
                                };
                            self.assign_to_local_slot(binding, result);
                        } else {
                            // Borrowed slot (param) or primitive: plain overwrite.
                            // The scope owns no ref here, so there is nothing to
                            // release and nothing to retain.
                            let result = self.compile_expr_result_as(&a.value, binding.shape)?;
                            self.assign_to_local_slot(binding, result);
                        }
                        Ok(())
                    }
                    Some(Resolve::Upvalue(uv_idx)) => {
                        let upv = self.upvalues[uv_idx as usize];
                        if !upv.is_cell {
                            return Err(BuildError::UnsupportedStatement(
                                "AssignmentStatement/write-to-snapshot-upvalue",
                            ));
                        }
                        // env[uv] stores the NaN-boxed cell (plan 114).
                        // Unbox the address, then value-RC store at @8.
                        let cell_addr = self.alloc_i32_local();
                        self.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
                        self.emit(Instruction::I64Load(mem_off(uv_idx as u64 * 8)));
                        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
                        self.emit(Instruction::LocalSet(cell_addr));
                        let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
                        self.emit_cell_store(cell_addr, result);
                        Ok(())
                    }
                    Some(Resolve::ModuleVar(global_idx)) => {
                        // A top-level `var` global owns its value for the life of
                        // the program (RC, plan 113 R1): retain a borrowed new
                        // value, release the previous one (reclaiming it mid-run),
                        // then store. The initial global value is 0/VAL_VOID, on
                        // which RT_RELEASE's is_obj guard is a safe no-op.
                        let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
                        self.assign_to_global_slot(global_idx, result);
                        Ok(())
                    }
                    None => Err(BuildError::UnknownIdentifier(names[0].clone())),
                }
            }
            AssignmentTarget::Variables { names } => {
                // Multi-variable assignment `a, b = swap(...)` —
                // destructure the RHS tuple into each existing
                // local. Each name must already be bound;
                // reassignment doesn't allocate new locals.
                let tuple_owned = self.expr_transfers_ownership(&a.value);
                self.compile_expr(&a.value)?;
                let tuple_local = self.alloc_local();
                self.emit(Instruction::LocalSet(tuple_local));
                for (i, name) in names.iter().enumerate() {
                    let Some(binding) = self.lookup(name) else {
                        return Err(BuildError::UnknownIdentifier(name.clone()));
                    };
                    self.emit(Instruction::LocalGet(tuple_local));
                    self.emit(Instruction::I32Const(i as i32));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
                    self.emit(Instruction::Call(self.rt().base + RT_GET_INDEX));
                    self.emit_convert(ValueShape::Boxed, binding.shape)?;
                    let result = match binding.shape {
                        ValueShape::Boxed => ExprResult {
                            shape: ValueShape::Boxed,
                            ownership: ExprOwnership::Borrowed,
                        },
                        shape => ExprResult::primitive(shape),
                    };
                    self.assign_to_local_slot(binding, result);
                }
                if tuple_owned {
                    self.release_owned_local(tuple_local, OwnershipOp::Discard);
                }
                Ok(())
            }
            AssignmentTarget::Field { object } => {
                // `obj.field = value`. The AST's `object` is the
                // full MemberExpression — decompose it to reach the
                // object and field name, intern the name, and
                // dispatch to `RT_SET_FIELD(obj, key_ptr, key_len, val)`.
                let me = match object.as_ref() {
                    Expression::MemberExpression(me) => me,
                    _ => {
                        return Err(BuildError::UnsupportedStatement(
                            "AssignmentStatement/Field-non-member",
                        ));
                    }
                };
                // Refuse assignment to a module alias — modules
                // aren't mutable bindings. A local/upvalue/module-var
                // whose name happens to collide with the module alias
                // (common when a parameter is named `signal` inside
                // the `signal` module) keeps its binding semantics
                // and flows through to field-store as a normal object.
                if let Expression::IdentifierExpression(obj_id) = &*me.object {
                    let shadowed_by_binding = self.resolve(&obj_id.name).is_some();
                    if !shadowed_by_binding
                        && (self.ctx.module_aliases.contains_key(&obj_id.name)
                            || obj_id.name == "assert")
                    {
                        return Err(BuildError::UnsupportedStatement(
                            "AssignmentStatement/Field-on-module",
                        ));
                    }
                }
                let (key_off, key_len) = self.ctx.strings.borrow_mut().intern(&me.property);
                self.compile_expr(&me.object)?;
                self.emit(Instruction::I32Const(key_off as i32));
                self.emit(Instruction::I32Const(key_len as i32));
                // Must be Boxed — record fields hold NaN-boxed values. A
                // RawInt/RawFloat from an arithmetic fast-path (e.g.
                // `s.val = s.val + 1`) stored unconverted would read back as
                // garbage, exactly as the Index path below guards against.
                let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
                self.store_field_value(result);
                // RT_SET_FIELD now returns the (possibly reallocated) dict
                // pointer. This `obj.field = v` statement path is used for
                // records/instances (fixed shape — never grow, pointer
                // unchanged) and the rare dict member-write; we can't rebind
                // an arbitrary lvalue here, so drop the result. String-keyed
                // dict growth goes through `dictionary.set`, which threads
                // the returned pointer.
                self.emit(Instruction::Drop);
                Ok(())
            }
            AssignmentTarget::Index { object } => {
                // `arr[i] = value`. The AST's `object` is the full
                // IndexExpression. We unbox the array address, add
                // `8 + i*8`, and do an `I64Store`. This mirrors the
                // bytecode path's `Op::SetIndex` — a direct memory
                // write with no bounds check (matches translator
                // semantics for parity).
                let ie = match object.as_ref() {
                    Expression::IndexExpression(ie) => ie,
                    _ => {
                        return Err(BuildError::UnsupportedStatement(
                            "AssignmentStatement/Index-non-index",
                        ));
                    }
                };
                // Compute the slot address `arr_addr + 8 + i*8` into a local so
                // we can both release the old occupant and store the new value
                // through it (RC, plan 113 R1).
                self.compile_expr_as(&ie.object, ValueShape::Boxed)?;
                self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
                let arr_addr = self.alloc_i32_local();
                self.emit(Instruction::LocalSet(arr_addr));
                self.compile_expr_as(&ie.index, ValueShape::RawInt)?;
                self.emit(Instruction::I32WrapI64);
                let idx = self.alloc_i32_local();
                self.emit(Instruction::LocalSet(idx));
                // Checked-mode (plan 116): an out-of-range index store is
                // silent heap corruption — i = -1 lands on the array's own
                // tag/count header; past-end clobbers the next block. Trap
                // with a named reason at the write site instead. Cheap
                // (one compare on a write that already happens) so it
                // rides along with `--checked`, not just FAI_RC_CHECK.
                if crate::runtime::checked_enabled() {
                    self.emit(Instruction::LocalGet(idx));
                    self.emit(Instruction::LocalGet(arr_addr));
                    self.emit(Instruction::I32Load(MemArg {
                        offset: 4,
                        align: 0,
                        memory_index: 0,
                    }));
                    self.emit(Instruction::I32GeU); // unsigned: negative idx → huge
                    self.emit_open(Instruction::If(BlockType::Empty));
                    self.emit(Instruction::I32Const(crate::runtime::TRAP_INDEX_OOB));
                    self.emit(Instruction::LocalGet(idx));
                    self.emit(Instruction::I64ExtendI32S);
                    self.emit(Instruction::LocalGet(arr_addr));
                    self.emit(Instruction::I32Load(MemArg {
                        offset: 4,
                        align: 0,
                        memory_index: 0,
                    }));
                    self.emit(Instruction::I64ExtendI32U);
                    self.emit_import_call(IMPORT_TRAP_REPORT);
                    self.emit(Instruction::Unreachable);
                    self.emit_close();
                }
                self.emit(Instruction::LocalGet(arr_addr));
                self.emit(Instruction::I32Const(8));
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalGet(idx));
                self.emit(Instruction::I32Const(8));
                self.emit(Instruction::I32Mul);
                self.emit(Instruction::I32Add);
                let slot = self.alloc_i32_local();
                self.emit(Instruction::LocalSet(slot));
                // Evaluate the new value and retain it (if borrowed) FIRST — it
                // may read the slot being overwritten (`xs[i] = xs[i]`), so the
                // old value must stay alive until after we've taken our ref.
                // Must be Boxed — array slots hold NaN-boxed values; a
                // RawInt/RawFloat stored unconverted would read back as garbage.
                let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
                self.store_index_slot(slot, result);
                Ok(())
            }
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.scope_drops.push(Vec::new());
    }

    fn cleanup_depth(&self) -> usize {
        self.scope_drops.len()
    }

    fn pop_scope(&mut self) {
        // Fall-through exit of this scope: free its confined fresh-literal
        // bindings. (On a `return` that jumped out of this scope, this code is
        // unreachable — the return already dropped these and diverged.)
        if let Some(drops) = self.scope_drops.last() {
            if !drops.is_empty() {
                // RC release (plan 113): decrement each confined local's count and
                // deep-free at zero. Refcounting makes this order-independent and
                // safe even when a value is co-owned (e.g. stored in a container) —
                // whoever releases last frees it.
                let locals = drops.clone();
                for l in locals {
                    self.release_owned_local(l, OwnershipOp::Cleanup);
                }
            }
        }
        self.scopes.pop();
        self.scope_drops.pop();
    }

    /// Record `local` as a scope-exit drop in the current (innermost) scope, if
    /// `value` is a confined fresh-literal allocation. Called at binding time.
    pub(super) fn note_droppable(&mut self, local: u32) {
        // RC scope-exit release (plan 113 R1): every object local owns exactly
        // one reference (transfer-fresh / retain-borrowed at the bind), so it is
        // released at scope exit. Refcounting makes this order-independent and
        // safe under co-ownership — the last owner frees. Callers gate on
        // `ValueShape::Boxed` (primitives carry no count).
        if let Some(top) = self.scope_drops.last_mut() {
            top.push(local);
        }
    }

    /// Emit `rt_drop` for every confined binding in scopes deeper than
    /// `target_depth` (innermost -> outermost). Used before any non-trap early
    /// exit that jumps past lexical `pop_scope` cleanup.
    fn emit_cleanup_to_depth(&mut self, target_depth: usize) {
        let locals: Vec<u32> = self
            .scope_drops
            .iter()
            .skip(target_depth)
            .rev()
            .flatten()
            .copied()
            .collect();
        if locals.is_empty() {
            return;
        }
        for l in locals {
            self.release_owned_local(l, OwnershipOp::Cleanup);
        }
    }

    /// Emit `rt_drop` for every confined binding in every active scope
    /// (innermost -> outermost). Used before a `return`/tail, which jumps past
    /// the `pop_scope` of each enclosing scope.
    fn emit_all_active_drops(&mut self) {
        // RC release before a return/tail (which jumps past each pop_scope).
        // `compile_return`/`compile_tail_stmt` already retained a borrowed return
        // value before calling this, so releasing its owning local here leaves it
        // alive at +1 for the caller to take ownership of.
        self.emit_cleanup_to_depth(0);
    }

    fn has_active_drops(&self) -> bool {
        self.scope_drops.iter().any(|s| !s.is_empty())
    }

    /// True if `local` is an object local this scope OWNS — i.e. it was
    /// registered via `note_droppable` and will be released at scope exit. Only
    /// owned locals carry the `+1` that makes reassignment release-the-old
    /// correct; borrowed slots (function params) own nothing, so releasing their
    /// previous value would free something the caller still owns (a UAF). Used
    /// by `compile_assignment` to decide whether to release-old / retain-new.
    fn is_owned_local(&self, local: u32) -> bool {
        self.scope_drops.iter().any(|s| s.contains(&local))
            || self.owned_frame_locals.contains(&local)
    }

    /// Tail statement: the value of this statement (if it's an
    /// expression) is the function's return value. Branches of an
    /// `if` in tail position are themselves tails — so each one
    /// emits its own `Return` (no wasm `if` result-type needed).
    fn compile_tail_stmt(&mut self, stmt: &Statement) -> Result<(), BuildError> {
        match stmt {
            Statement::ExpressionStatement(es) => self.return_value(Some(&es.expression)),
            Statement::TryStatement(s) => self.compile_try_as_tail(s),
            Statement::IfStatement(s) => {
                self.compile_if_branches_tail(&s.branches, s.else_branch.as_deref())?;
                // Fall-through safety: if no branch matched and
                // there's no else, return void. In practice all
                // reachable paths inside the branches already emit
                // Return, so this is unreachable code that wasm's
                // polymorphic-Return rules let us emit safely.
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit_debug_function_end();
                self.emit(Instruction::Return);
                Ok(())
            }
            Statement::CaseStatement(s) => {
                self.compile_case(s, true)?;
                // Same fall-through safety as IfStatement above — if
                // no `when` branch matched and the source has no
                // `else`, return Void. Normally unreachable since
                // each tail branch emits `Return`, but wasm's
                // polymorphic-Return rules let the trailer land
                // safely after the structured If tower.
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit_debug_function_end();
                self.emit(Instruction::Return);
                Ok(())
            }
            _ => {
                // Non-expression tail (a trailing `var`/`let`, UseStatement in
                // main, etc.): compile as side effect, release the confined
                // locals in every active scope (RC scope-exit on fall-through;
                // the expression-tail arm above does the same), return Void.
                self.compile_stmt(stmt)?;
                self.emit_all_active_drops();
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit_debug_function_end();
                self.emit(Instruction::Return);
                Ok(())
            }
        }
    }

    /// Compile a sequence of statements where the last one is a tail
    /// (its value becomes the enclosing function's return value).
    /// Used inside `if` branches when the `if` itself is tail.
    fn compile_stmts_as_tail(&mut self, stmts: &[Statement]) -> Result<(), BuildError> {
        if stmts.is_empty() {
            self.emit(Instruction::I64Const(VAL_VOID));
            self.emit_debug_function_end();
            self.emit(Instruction::Return);
            return Ok(());
        }
        let last = stmts.len() - 1;
        for (i, s) in stmts.iter().enumerate() {
            if i == last {
                self.compile_tail_stmt(s)?;
            } else {
                self.compile_stmt(s)?;
            }
        }
        Ok(())
    }

    /// Mirror of `compile_if_branches` but every branch body is a
    /// tail — each one ends in its own `Return`.
    fn compile_if_branches_tail(
        &mut self,
        branches: &[fai_compiler::ast::IfBranch],
        else_branch: Option<&[Statement]>,
    ) -> Result<(), BuildError> {
        if branches.is_empty() {
            if let Some(body) = else_branch {
                self.push_scope();
                self.compile_stmts_as_tail(body)?;
                self.pop_scope();
            } else {
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit_debug_function_end();
                self.emit(Instruction::Return);
            }
            return Ok(());
        }
        let first = &branches[0];
        self.compile_truthy_i32(&first.condition)?;
        self.emit_open(Instruction::If(BlockType::Empty));
        self.push_scope();
        self.compile_stmts_as_tail(&first.body)?;
        self.pop_scope();
        if branches.len() > 1 || else_branch.is_some() {
            self.emit(Instruction::Else);
            self.compile_if_branches_tail(&branches[1..], else_branch)?;
        }
        self.emit_close();
        Ok(())
    }

    fn compile_let(&mut self, s: &LetStatement) -> Result<(), BuildError> {
        self.compile_bindings(&s.bindings, &s.value, s.is_shared.unwrap_or(false))
    }

    fn compile_var(&mut self, s: &VarStatement) -> Result<(), BuildError> {
        self.compile_bindings(&s.bindings, &s.value, s.is_shared.unwrap_or(false))
    }

    /// Shared binding logic for `let` and `var`. The direct path
    /// treats them identically at the wasm level — both allocate a
    /// fresh local and bind; `var`'s mutability is enforced upstream
    /// by the checker, not here.
    ///
    /// Multi-binding (`let a, b = rhs`) evaluates `rhs` once into a
    /// local (expected to be a Tuple), then destructures by reading
    /// each index via `RT_GET_INDEX(tuple, MAKE_INT(i))`. Mirrors
    /// the bytecode compiler's per-element `Op::GetIndex` loop.
    fn compile_bindings(
        &mut self,
        bindings: &[fai_compiler::ast::BindingDeclaration],
        value: &Expression,
        is_shared: bool,
    ) -> Result<(), BuildError> {
        match bindings.len() {
            0 => Err(BuildError::UnsupportedStatement(
                "LetStatement/empty-bindings",
            )),
            1 => {
                let name = &bindings[0].name;

                // `let t Type = from_dict(dict)` — expand at compile
                // time to the equivalent type constructor call using
                // the declared fields. Driving the type from the LHS
                // annotation lets the call stay a one-liner while the
                // codegen gets to statically resolve every field.
                if let Some(annotation) = &bindings[0].type_name {
                    if let Some(type_name) = annotation.name.clone() {
                        if let Expression::CallExpression(ce) = value {
                            if let Expression::IdentifierExpression(id) = &*ce.callee {
                                if id.name == "query_typed"
                                    && ce.args.len() == 3
                                    && annotation.is_array
                                    && self.ctx.type_fields.contains_key(&type_name)
                                {
                                    self.compile_query_typed_binding(name, &type_name, ce)?;
                                    return Ok(());
                                }
                                if id.name == "from_dict" && ce.args.len() == 1 {
                                    if self.ctx.type_fields.contains_key(&type_name) {
                                        let dict_expr = ce.args[0].value.clone();
                                        self.compile_from_dict_binding(
                                            name, &type_name, dict_expr,
                                        )?;
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }

                // Cell-box vars that are captured by a nested closure.
                // The pre-pass populated `cell_captured_vars` with only
                // `var` names (not `let`), so this never trips on an
                // immutable binding.
                if self.cell_captured_vars.contains(name) {
                    // Already bound as a cell? In a resume fn the frame slot
                    // holds the boxed pointer of the heap cell, seeded at
                    // function entry — store the initial value *through* that
                    // existing cell rather than allocating a fresh one and
                    // rebinding. Rebinding would orphan the frame's cell
                    // (losing the value across suspension) and hand the
                    // capturing closure a plain i64 local where it expects an
                    // i32 cell address.
                    if let Some(existing) = self.lookup(name) {
                        if existing.is_cell {
                            let addr_local = existing.local;
                            let result = self.compile_expr_result_as(value, ValueShape::Boxed)?;
                            self.emit_cell_store(addr_local, result);
                            return Ok(());
                        }
                    }
                    // Allocate a tagged 16-byte cell (plan 114), store the
                    // (Boxed) initial value with value-RC, bind the name to
                    // a cell binding. Reads and writes on either side deref
                    // the value slot at offset 8.
                    let addr_local = self.emit_cell_alloc();
                    let result = self.compile_expr_result_as(value, ValueShape::Boxed)?;
                    self.emit_cell_store(addr_local, result);
                    self.bind_cell(name, addr_local);
                    // The scope owns the cell's +1 from the allocator:
                    // release it at scope exit like any owned binding (the
                    // shadow local carries the boxed form scope_drops
                    // expects). A capturing closure that escapes keeps the
                    // cell alive through its own retained upvalue ref —
                    // before plan 114 this block simply leaked.
                    let boxed_local = self.alloc_local();
                    self.emit(Instruction::LocalGet(addr_local));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
                    self.emit(Instruction::LocalSet(boxed_local));
                    self.note_droppable(boxed_local);
                    return Ok(());
                }

                // When the binding carries an explicit type annotation
                // (`let x Float = 0`, `let x Int = 3.7`), the declared
                // type wins over the value's inferred type — emit_convert
                // handles the Int↔Float widening/narrowing the checker
                // approved. Otherwise fall back to the value's shape.
                let shape = bindings[0]
                    .type_name
                    .as_ref()
                    .map(shape_for_type_node)
                    .unwrap_or_else(|| self.shape_for_expr(value));
                let result = self.compile_expr_result_as(value, shape)?;
                // RC bind (plan 113 R1): `bind_to_local` retains borrowed boxed
                // values, transfers owned boxed values, and registers owning
                // boxed locals for scope-exit release.
                self.bind_to_local(name, result, !is_shared);
                Ok(())
            }
            _ => {
                // Evaluate the RHS (expected Tuple) into a scratch
                // local so we can index into it repeatedly.
                let tuple_owned = self.expr_transfers_ownership(value);
                self.compile_expr_as(value, ValueShape::Boxed)?;
                let tuple_local = self.alloc_local();
                self.emit(Instruction::LocalSet(tuple_local));

                for (i, binding) in bindings.iter().enumerate() {
                    // RT_GET_INDEX(tuple, NaN-boxed-Int(i)) — reads
                    // entry `i`. Works for any container tag; for
                    // Tuples (tag=2) the layout matches Arrays so
                    // the helper returns the stored i64 directly.
                    self.emit(Instruction::LocalGet(tuple_local));
                    self.emit(Instruction::I32Const(i as i32));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
                    self.emit(Instruction::Call(self.rt().base + RT_GET_INDEX));
                    self.bind_to_local(
                        &binding.name,
                        ExprResult {
                            shape: ValueShape::Boxed,
                            ownership: ExprOwnership::Borrowed,
                        },
                        true,
                    );
                }
                if tuple_owned {
                    self.release_owned_local(tuple_local, OwnershipOp::Discard);
                }
                Ok(())
            }
        }
    }
}
