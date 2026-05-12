//! Program- and module-level setup: imports, forward declarations, toposort.

use std::collections::{HashMap, HashSet};

use fai_compiler::ast::*;

use super::{Checker, PreparedModule};
use crate::builtins;
use crate::environment::Environment;
use crate::error::CheckError;
use crate::std_modules;
use crate::types::*;

impl Checker {
    /// Check a single-module program (the entry point).
    pub fn check_program(&mut self, statements: &[Statement]) -> Result<(), CheckError> {
        // Phase 1: collect type and enum declarations
        self.collect_declarations(statements)?;
        // Phase 2: resolve type fields
        self.resolve_type_fields()?;
        // Phase 3: check all statements
        let mut env = Environment::new();
        self.install_builtins(&mut env)?;
        // Install std imports for the entry module
        let empty_exports = HashMap::new();
        self.install_imports(statements, &mut env, &empty_exports, None)?;
        self.check_top_level_statements(statements, &mut env)?;
        self.finish_check()
    }

    /// Check a program with modules.
    pub fn check_with_modules(
        &mut self,
        entry_statements: &[Statement],
        modules: &[PreparedModule],
    ) -> Result<(), CheckError> {
        // Collect declarations from all modules + entry
        for module in modules {
            self.collect_declarations(&module.statements)?;
        }
        self.collect_declarations(entry_statements)?;

        // Resolve type fields
        self.resolve_type_fields()?;

        // Topologically sort modules so dependencies are checked before dependents
        let sorted_indices = self.toposort_modules(modules);

        // Seed the cross-module export table FROM DECLARATIONS before we
        // body-check anything. This breaks import cycles: module A doing
        // `use { Team } from server` can resolve `Team` even when module
        // `server`'s body hasn't been checked yet (or itself imports from
        // A). Without this, the per-module loop populated exports only
        // after each body check — so any cycle produced cascades of
        // `Unknown name` errors across both modules.
        //
        // The seeded entries cover the static surface: types, enums,
        // function signatures, extern types/functions, function typedef
        // aliases. Module-level `let`/`var` values (whose types are only
        // known after body check) are merged in afterwards per module.
        let mut module_type_exports: HashMap<String, HashMap<String, Type>> = HashMap::new();
        for module in modules {
            let seeded = self.seed_module_exports(module)?;
            module_type_exports.insert(module.name.clone(), seeded);
        }

        // Check modules (in dependency order when possible; cycles are
        // handled by the seeded exports above).
        for &idx in &sorted_indices {
            let module = &modules[idx];
            let mut env = Environment::new();
            self.install_builtins(&mut env)?;

            // Install imports for this module
            self.install_imports(
                &module.statements,
                &mut env,
                &module_type_exports,
                Some(&module.name),
            )?;

            // Forward-declare all functions/types/enums (public and private within module)
            self.forward_declare_all(&module.statements, &mut env)?;

            // Forward-declare module-level `var` / `let` bindings before
            // any function body is checked. Without this, a `var` defined
            // in one file of the module isn't visible to a function in
            // another file checked earlier in source order. The pre-pass
            // type-checks each initializer and adds the name to env;
            // the main loop below skips bindings that are already there.
            self.forward_declare_module_bindings(&module.statements, &module.file_paths, &mut env);

            // Mark the module currently being checked so that any
            // ufcs_calls / named_param_reorder entries get tagged with
            // this module's name. This prevents source-coordinate
            // collisions across files from stomping each other's
            // metadata.
            self.current_module = Some(module.name.clone());

            // Check each statement, accumulating errors per statement so
            // one error in a module doesn't hide the rest (same pattern as
            // check_top_level_statements below). `current_file` is
            // updated per statement so error messages carry the real
            // file path even though one module's statements span
            // multiple files.
            for (idx, stmt) in module.statements.iter().enumerate() {
                self.current_file = module
                    .file_paths
                    .get(idx)
                    .cloned()
                    .flatten()
                    .or_else(|| module.file_path.clone());
                if let Err(e) = self.check_top_level_statement(stmt, &mut env) {
                    install_failed_binding_placeholders(stmt, &mut env);
                    self.collected_errors.push(e);
                }
            }
            self.current_file = None;
            self.current_module = None;

            // Merge env-derived exports (catches module-level let/var
            // bindings whose types are only known post-check) into the
            // already-seeded entry. Keeping the seeded entry means
            // a body-check failure in this module doesn't drop the
            // static-signature entries other modules are importing.
            let exports = module_type_exports.entry(module.name.clone()).or_default();
            self.collect_module_exports(&module.statements, &module.private_names, &env, exports)?;
        }

        // Check entry module
        let mut env = Environment::new();
        self.install_builtins(&mut env)?;
        self.install_imports(entry_statements, &mut env, &module_type_exports, None)?;
        // Entry module uses `current_module = None` (or equivalently ""),
        // which matches the compiler's empty module_prefix for the entry.
        self.current_module = None;
        self.check_top_level_statements(entry_statements, &mut env)?;
        self.finish_check()
    }

    /// If any statement-level errors were accumulated, return the first
    /// one via Result so existing callers keep working. The full list
    /// stays accessible on `checker.collected_errors` so CLI/tooling that
    /// wants to print every error can iterate it after the call.
    fn finish_check(&mut self) -> Result<(), CheckError> {
        if let Some(first) = self.collected_errors.first() {
            return Err(CheckError {
                message: first.message.clone(),
                file: first.file.clone(),
                line: first.line,
                column: first.column,
            });
        }
        Ok(())
    }

    /// Topologically sort modules by their use-statement dependencies.
    /// Modules with no dependencies on other app modules come first.
    fn toposort_modules(&self, modules: &[PreparedModule]) -> Vec<usize> {
        let module_names: HashMap<&str, usize> = modules
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.as_str(), i))
            .collect();

        // Build adjacency: deps[i] = set of module indices that module i depends on
        let mut deps: Vec<HashSet<usize>> = Vec::with_capacity(modules.len());
        for module in modules {
            let mut module_deps = HashSet::new();
            for stmt in &module.statements {
                if let Statement::UseStatement(use_stmt) = stmt {
                    let dep_name =
                        Self::qualify_module_path(Some(&module.name), &use_stmt.module_path);
                    if let Some(&dep_idx) = module_names.get(dep_name.as_str()) {
                        module_deps.insert(dep_idx);
                    }
                }
            }
            deps.push(module_deps);
        }

        // Kahn's algorithm.
        // in_degree[i] = number of modules that i depends on (i must come after its deps)
        let mut in_degree: Vec<usize> = vec![0; modules.len()];
        for (i, module_deps) in deps.iter().enumerate() {
            in_degree[i] = module_deps.len();
        }

        let mut queue: Vec<usize> = (0..modules.len()).filter(|&i| in_degree[i] == 0).collect();
        let mut sorted = Vec::with_capacity(modules.len());

        while let Some(node) = queue.pop() {
            sorted.push(node);
            // Find all modules that depend on this node
            for (i, module_deps) in deps.iter().enumerate() {
                if module_deps.contains(&node) {
                    in_degree[i] -= 1;
                    if in_degree[i] == 0 {
                        queue.push(i);
                    }
                }
            }
        }

        // If there's a cycle, append remaining modules in original order
        if sorted.len() < modules.len() {
            for i in 0..modules.len() {
                if !sorted.contains(&i) {
                    sorted.push(i);
                }
            }
        }

        sorted
    }

    fn install_builtins(&self, env: &mut Environment) -> Result<(), CheckError> {
        for (name, ty) in &self.builtins {
            env.define(name, ty.clone(), false)?;
        }
        // Install assert namespace
        let assert_ns = builtins::assert_namespace(&self.builtins);
        env.define("assert", assert_ns, false)?;
        Ok(())
    }

    fn collect_declarations(&mut self, statements: &[Statement]) -> Result<(), CheckError> {
        for stmt in statements {
            match stmt {
                Statement::TypeDeclaration(td) => {
                    self.type_declarations.insert(td.name.clone(), td.clone());
                }
                Statement::EnumDeclaration(ed) => {
                    self.enum_declarations
                        .insert(ed.name.clone(), ed.members.clone());
                }
                Statement::FunctionTypeDefDeclaration(ftd) => {
                    // Register as a function type in builtins so it can be used as a type name
                    let params: Vec<FunctionParam> = ftd
                        .params
                        .iter()
                        .map(|p| {
                            let ty = self
                                .resolve_type_node(&p.type_node)
                                .unwrap_or(Type::Unknown);
                            FunctionParam {
                                name: p.name.clone(),
                                ty,
                                has_default: false,
                                is_mutable: p.is_mutable,
                            }
                        })
                        .collect();
                    let returns: Vec<Type> = ftd
                        .return_types
                        .iter()
                        .map(|r| {
                            self.resolve_type_node(&r.type_node)
                                .unwrap_or(Type::Unknown)
                        })
                        .collect();
                    let sig = FunctionSig {
                        name: ftd.name.clone(),
                        type_params: Vec::new(),
                        params,
                        returns,
                    };
                    self.builtins.insert(ftd.name.clone(), Type::Function(sig));
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn resolve_type_fields(&mut self) -> Result<(), CheckError> {
        // Error type has built-in fields
        let mut error_fields = HashMap::new();
        error_fields.insert("message".to_string(), Type::String);
        error_fields.insert("type".to_string(), Type::String);
        self.type_fields.insert("Error".to_string(), error_fields);

        // Response type (returned by std.http.request methods)
        let mut response_fields = HashMap::new();
        response_fields.insert("status".to_string(), Type::Int);
        response_fields.insert("body".to_string(), Type::String);
        response_fields.insert("headers".to_string(), Type::Dictionary);
        self.type_fields
            .insert("Response".to_string(), response_fields);

        // HttpRequest type (passed to HTTP server route handlers)
        let mut http_request_fields = HashMap::new();
        http_request_fields.insert("method".to_string(), Type::String);
        http_request_fields.insert("path".to_string(), Type::String);
        http_request_fields.insert("body".to_string(), Type::String);
        http_request_fields.insert("headers".to_string(), Type::Dictionary);
        self.type_fields
            .insert("HttpRequest".to_string(), http_request_fields);

        // Router type — no declared fields; all methods accessed via UFCS
        self.type_fields
            .insert("Router".to_string(), HashMap::new());

        // Event type (passed to handlers registered via std.events.on).
        // `data` is `Unknown` so emitters pass any value; subscribers
        // recover a typed view via `let x T = from_dict(event.data)`.
        let mut event_fields = HashMap::new();
        event_fields.insert("name".to_string(), Type::String);
        event_fields.insert("data".to_string(), Type::Unknown);
        self.type_fields.insert("Event".to_string(), event_fields);

        // Subscription type — the handle returned by `std.events.on`/
        // `once`. Self-describing so `off(sub)` works without the
        // caller knowing which event name the subscription targets.
        let mut subscription_fields = HashMap::new();
        subscription_fields.insert("id".to_string(), Type::Int);
        subscription_fields.insert("name".to_string(), Type::String);
        self.type_fields
            .insert("Subscription".to_string(), subscription_fields);

        // HttpResponse type — what HTTP route handlers and RPC
        // handlers return. Replaces the legacy Dictionary-shaped
        // response. Optional fields (`contentType`, `location`,
        // `cookies`, `headers`) are read by the host serializer when
        // present and skipped when absent.
        let mut http_response_fields = HashMap::new();
        http_response_fields.insert("status".to_string(), Type::Int);
        http_response_fields.insert("body".to_string(), Type::String);
        http_response_fields.insert("contentType".to_string(), optional_of(Type::String));
        http_response_fields.insert("location".to_string(), optional_of(Type::String));
        http_response_fields.insert(
            "cookies".to_string(),
            optional_of(array_of(named_type("Cookie", NamedCategory::Type))),
        );
        http_response_fields.insert("headers".to_string(), optional_of(Type::Dictionary));
        self.type_fields
            .insert("HttpResponse".to_string(), http_response_fields);

        // Cookie type — Set-Cookie attributes the host serializer
        // emits onto the wire. Path/maxAge/httpOnly/secure/sameSite
        // are optional; only `name` and `value` are required.
        let mut cookie_fields = HashMap::new();
        cookie_fields.insert("name".to_string(), Type::String);
        cookie_fields.insert("value".to_string(), Type::String);
        cookie_fields.insert("path".to_string(), optional_of(Type::String));
        cookie_fields.insert("maxAge".to_string(), optional_of(Type::Int));
        cookie_fields.insert("httpOnly".to_string(), optional_of(Type::Bool));
        cookie_fields.insert("secure".to_string(), optional_of(Type::Bool));
        cookie_fields.insert("sameSite".to_string(), optional_of(Type::String));
        self.type_fields.insert("Cookie".to_string(), cookie_fields);

        // Standard event payload shapes — registered as type_fields so
        // subscribers can `let x T = from_dict(event.data)` to recover
        // a typed view of the payload. Emitters (host or codegen-side)
        // pass Dict-shaped values matching these field sets.

        // RequestResponse — http:afterResponse payload.
        let mut request_response_fields = HashMap::new();
        request_response_fields.insert(
            "request".to_string(),
            named_type("HttpRequest", NamedCategory::Type),
        );
        request_response_fields.insert(
            "response".to_string(),
            named_type("HttpResponse", NamedCategory::Type),
        );
        self.type_fields
            .insert("RequestResponse".to_string(), request_response_fields);

        // ServerStarted — http:listening payload.
        let mut server_started_fields = HashMap::new();
        server_started_fields.insert("port".to_string(), Type::Int);
        self.type_fields
            .insert("ServerStarted".to_string(), server_started_fields);

        // HttpError — http:error payload (handler threw).
        let mut http_error_fields = HashMap::new();
        http_error_fields.insert(
            "request".to_string(),
            named_type("HttpRequest", NamedCategory::Type),
        );
        http_error_fields.insert("message".to_string(), Type::String);
        self.type_fields
            .insert("HttpError".to_string(), http_error_fields);

        // RpcCall — rpc:beforeCall payload.
        let mut rpc_call_fields = HashMap::new();
        rpc_call_fields.insert("fnName".to_string(), Type::String);
        rpc_call_fields.insert("args".to_string(), Type::String);
        self.type_fields
            .insert("RpcCall".to_string(), rpc_call_fields);

        // RpcResult — rpc:afterCall payload.
        let mut rpc_result_fields = HashMap::new();
        rpc_result_fields.insert("fnName".to_string(), Type::String);
        rpc_result_fields.insert("value".to_string(), Type::String);
        self.type_fields
            .insert("RpcResult".to_string(), rpc_result_fields);

        // RpcError — rpc:error payload.
        let mut rpc_error_fields = HashMap::new();
        rpc_error_fields.insert("fnName".to_string(), Type::String);
        rpc_error_fields.insert("message".to_string(), Type::String);
        self.type_fields
            .insert("RpcError".to_string(), rpc_error_fields);

        // Resolve fields for all type declarations
        let decls: Vec<_> = self.type_declarations.values().cloned().collect();
        for decl in &decls {
            let mut fields = HashMap::new();
            for field in &decl.fields {
                let ft = self.resolve_type_node(&field.type_node)?;
                if fields.contains_key(&field.name) {
                    return Err(CheckError::new(format!(
                        "Duplicate field '{}' in type '{}'",
                        field.name, decl.name
                    )));
                }
                fields.insert(field.name.clone(), ft);
            }
            self.type_fields.insert(decl.name.clone(), fields);
        }
        Ok(())
    }

    fn install_imports(
        &self,
        statements: &[Statement],
        env: &mut Environment,
        module_exports: &HashMap<String, HashMap<String, Type>>,
        current_module_name: Option<&str>,
    ) -> Result<(), CheckError> {
        for stmt in statements {
            if let Statement::UseStatement(use_stmt) = stmt {
                if std_modules::is_std_module(&use_stmt.module_path) {
                    self.install_std_import(use_stmt, env)?;
                } else {
                    self.install_module_import(use_stmt, env, module_exports, current_module_name)?;
                }
            }
        }
        Ok(())
    }

    fn install_std_import(
        &self,
        use_stmt: &UseStatement,
        env: &mut Environment,
    ) -> Result<(), CheckError> {
        let module_name = std_modules::std_module_name(&use_stmt.module_path);
        let exports = self
            .std_exports
            .get(&module_name)
            .ok_or_else(|| CheckError::new(format!("Unknown standard module '{}'", module_name)))?;

        if use_stmt.import_all {
            for (export_name, builtin_name) in exports {
                let ty = self.builtins.get(builtin_name).ok_or_else(|| {
                    CheckError::new(format!(
                        "Standard module '{}' export '{}' is not implemented",
                        module_name, export_name
                    ))
                })?;
                self.install_glob_name(env, &module_name, export_name, ty)?;
            }
            return Ok(());
        }

        if let Some(imported_names) = &use_stmt.imported_names {
            if !imported_names.is_empty() {
                for name in imported_names {
                    let builtin_name = exports
                        .iter()
                        .find(|(export_name, _)| export_name == name)
                        .map(|(_, bn)| bn.as_str())
                        .ok_or_else(|| {
                            CheckError::new(format!(
                                "Standard module '{}' does not export '{}'. \
                                 Run `fai doc {}` to find which module exports it \
                                 (or whether it exists at all). \
                                 `fai doc {}` lists everything '{}' exports.",
                                module_name, name, name, module_name, module_name
                            ))
                        })?;
                    let ty = self.builtins.get(builtin_name).ok_or_else(|| {
                        CheckError::new(format!(
                            "Standard module '{}' export '{}' is not implemented",
                            module_name, name
                        ))
                    })?;
                    // Multiple files in a directory module may import the same name
                    let _ = env.define(name, ty.clone(), false);
                }
                return Ok(());
            }
        }

        // Namespace import
        let namespace_name = use_stmt.module_path.last().unwrap();
        let mut ns_exports = HashMap::new();
        for (export_name, builtin_name) in exports {
            let ty = self.builtins.get(builtin_name).ok_or_else(|| {
                CheckError::new(format!(
                    "Standard module '{}' export '{}' is not implemented",
                    module_name, export_name
                ))
            })?;
            ns_exports.insert(export_name.clone(), ty.clone());
        }
        let _ = env.define(
            namespace_name,
            Type::ModuleNamespace {
                name: module_name,
                exports: ns_exports,
            },
            false,
        );
        Ok(())
    }

    fn install_module_import(
        &self,
        use_stmt: &UseStatement,
        env: &mut Environment,
        module_exports: &HashMap<String, HashMap<String, Type>>,
        current_module_name: Option<&str>,
    ) -> Result<(), CheckError> {
        let module_name = Self::qualify_module_path(current_module_name, &use_stmt.module_path);
        let exports = match module_exports.get(&module_name) {
            Some(e) => e,
            None => {
                // Module not found in type exports — skip silently (may be checked later)
                return Ok(());
            }
        };

        if use_stmt.import_all {
            let mut names: Vec<&String> = exports.keys().collect();
            names.sort();
            for name in names {
                let ty = exports.get(name).unwrap();
                self.install_glob_name(env, &module_name, name, ty)?;
            }
            return Ok(());
        }

        if let Some(imported_names) = &use_stmt.imported_names {
            if !imported_names.is_empty() {
                for name in imported_names {
                    let ty = exports.get(name).ok_or_else(|| {
                        CheckError::new(format!(
                            "Module '{}' does not export '{}'. \
                             Run `fai doc {}` to find which module exports it \
                             (or whether it exists at all). \
                             `fai doc {}` lists everything '{}' exports.",
                            module_name, name, name, module_name, module_name
                        ))
                    })?;
                    // Multiple files in a directory module may import the same name;
                    // skip if already defined with the same type.
                    let _ = env.define(name, ty.clone(), false);
                }
                return Ok(());
            }
        }

        // Namespace import
        let namespace_name = use_stmt.module_path.last().unwrap();
        let _ = env.define(
            namespace_name,
            Type::ModuleNamespace {
                name: module_name,
                exports: exports.clone(),
            },
            false,
        );
        Ok(())
    }

    fn install_glob_name(
        &self,
        env: &mut Environment,
        module_name: &str,
        name: &str,
        ty: &Type,
    ) -> Result<(), CheckError> {
        if let Ok(existing) = env.get(name) {
            if same_type(&existing.ty, ty) {
                // Multiple files in one directory module often import the same UI
                // helpers. Treat repeated same-type imports as idempotent, matching
                // explicit named-import behavior.
                return Ok(());
            }
            return Err(CheckError::new(format!(
                "Glob import from '{}' cannot import '{}' because a different '{}' is already in scope. \
                 This usually means another import or declaration in this module uses the same name. \
                 Fix it by replacing `use * from {}` with an explicit `use {{ ... }} from {}` list \
                 that omits '{}', or rename the local declaration.",
                module_name, name, name, module_name, module_name, name
            )));
        }
        env.define(name, ty.clone(), false)
    }

    fn qualify_module_path(current_module_name: Option<&str>, module_path: &[String]) -> String {
        if module_path.first().map(|s| s.as_str()) == Some("std") {
            return module_path.join(".");
        }
        let is_external = module_path
            .first()
            .map(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
            .unwrap_or(false);
        if is_external {
            return module_path.join(".");
        }
        if let Some(current) = current_module_name {
            let package = current.split('.').next().unwrap_or(current);
            let is_package = package
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false);
            if is_package {
                return format!("{}.{}", package, module_path.join("."));
            }
        }
        module_path.join(".")
    }

    fn forward_declare_all(
        &mut self,
        statements: &[Statement],
        env: &mut Environment,
    ) -> Result<(), CheckError> {
        // First pass: type constructors and enum namespaces (all, public and private)
        for stmt in statements {
            match stmt {
                Statement::TypeDeclaration(td) => {
                    let _ = env.define(&td.name, Type::TypeConstructor(td.name.clone()), false);
                }
                Statement::EnumDeclaration(ed) => {
                    let _ = env.define(&ed.name, Type::EnumNamespace(ed.name.clone()), false);
                }
                Statement::ExternBlockDeclaration(ext) => {
                    // Register extern opaque types as Ptr
                    for t in &ext.types {
                        let _ = env.define(&t.name, Type::Ptr(t.name.clone()), false);
                        self.extern_types.insert(t.name.clone());
                    }
                }
                Statement::FunctionTypeDefDeclaration(ftd) => {
                    // Register as a named function type
                    if let Some(fn_type) = self.builtins.get(&ftd.name) {
                        let _ = env.define(&ftd.name, fn_type.clone(), false);
                    }
                }
                _ => {}
            }
        }

        // Second pass: function signatures (all, public and private).
        // Always define a name for each function even if its signature
        // can't be resolved — otherwise a single bad @param type would
        // cascade into "Unknown name" errors at every call site later
        // in the file, hiding the real error and (worse) preventing
        // compiler-generated declarations like `addRpcRoutes` from
        // being registered at all. The original resolve error is
        // captured so the user still sees it via collected_errors.
        for stmt in statements {
            if let Statement::FunctionDeclaration(fd) = stmt {
                match self.function_type_from_decl(fd) {
                    Ok(fn_type) => {
                        let _ = env.define(&fd.name, fn_type, false);
                    }
                    Err(e) => {
                        let _ = env.define(&fd.name, Type::Unknown, false);
                        self.collected_errors
                            .push(self.attach_location(e, &fd.location));
                    }
                }
            }
            // Register extern functions with their signatures
            if let Statement::ExternBlockDeclaration(ext) = stmt {
                for f in &ext.functions {
                    let mut params = Vec::new();
                    let mut had_param_err = false;
                    for p in &f.params {
                        // `out` params accept any type (the register is written, not read)
                        let ty = if p.is_out {
                            Type::Unknown
                        } else {
                            match self.resolve_type_node(&p.type_node) {
                                Ok(t) => t,
                                Err(e) => {
                                    self.collected_errors.push(e);
                                    had_param_err = true;
                                    Type::Unknown
                                }
                            }
                        };
                        params.push(crate::types::FunctionParam {
                            name: p.name.clone(),
                            ty,
                            has_default: p.default_value.is_some(),
                            is_mutable: p.is_mutable,
                        });
                    }
                    let _ = had_param_err; // silence unused warning — captured via collected_errors
                    let returns = if let Some(rt) = &f.return_type {
                        match self.resolve_type_node(rt) {
                            Ok(t) => vec![t],
                            Err(e) => {
                                self.collected_errors.push(e);
                                vec![Type::Unknown]
                            }
                        }
                    } else {
                        vec![Type::Void]
                    };
                    let fn_name = format!("{}.{}", ext.library, f.name);
                    let fn_type = Type::Function(crate::types::FunctionSig {
                        name: fn_name,
                        type_params: Vec::new(),
                        params,
                        returns,
                    });
                    let _ = env.define(&f.name, fn_type, false);
                }
            }
        }

        Ok(())
    }

    /// Type-check each top-level `var` / `let` binding and register the
    /// resulting name in the env. Run after `forward_declare_all` and
    /// before the main statement check loop. Errors are pushed onto
    /// `collected_errors` rather than returned so a single bad
    /// initializer doesn't abort the rest of the pre-pass.
    ///
    /// The main loop's `check_top_level_statement` for VarStatement /
    /// LetStatement no-ops when the binding name is already present in
    /// env, so this pre-pass is the single place each module-level
    /// binding gets checked.
    fn forward_declare_module_bindings(
        &mut self,
        statements: &[Statement],
        file_paths: &[Option<String>],
        env: &mut Environment,
    ) {
        for (idx, stmt) in statements.iter().enumerate() {
            self.current_file = file_paths.get(idx).cloned().flatten();
            match stmt {
                Statement::VarStatement(vs) => {
                    if let Err(e) =
                        self.check_binding_statement(&vs.bindings, &vs.value, env, true, "var")
                    {
                        install_failed_binding_placeholders(stmt, env);
                        self.collected_errors
                            .push(self.attach_location(e, &vs.location));
                    }
                }
                Statement::LetStatement(ls) => {
                    if let Err(e) =
                        self.check_binding_statement(&ls.bindings, &ls.value, env, false, "let")
                    {
                        install_failed_binding_placeholders(stmt, env);
                        self.collected_errors
                            .push(self.attach_location(e, &ls.location));
                    }
                }
                _ => {}
            }
        }
        self.current_file = None;
    }

    /// Build a module's public export table purely from its AST, no
    /// body check required. Mirrors `forward_declare_all` for signatures
    /// but writes into a map instead of an Environment so the result can
    /// be used by *other* modules' import resolution before any module
    /// body is visited. This is the key piece that makes cyclic module
    /// imports type-check: every module sees every other module's static
    /// surface up front.
    ///
    /// Excludes declarations marked private via the module's
    /// `private_names` list. `let`/`var` at module top level are skipped
    /// because their types aren't known until the initializer runs
    /// through the expression checker — those are filled in post-check
    /// by `collect_module_exports` merging into the seeded map.
    fn seed_module_exports(
        &mut self,
        module: &PreparedModule,
    ) -> Result<HashMap<String, Type>, CheckError> {
        let privates: HashSet<&str> = module.private_names.iter().map(|s| s.as_str()).collect();
        let mut exports: HashMap<String, Type> = HashMap::new();

        for stmt in &module.statements {
            match stmt {
                Statement::TypeDeclaration(td) if !privates.contains(td.name.as_str()) => {
                    exports.insert(td.name.clone(), Type::TypeConstructor(td.name.clone()));
                }
                Statement::EnumDeclaration(ed) if !privates.contains(ed.name.as_str()) => {
                    exports.insert(ed.name.clone(), Type::EnumNamespace(ed.name.clone()));
                }
                Statement::FunctionTypeDefDeclaration(ftd)
                    if !privates.contains(ftd.name.as_str()) =>
                {
                    if let Some(fn_type) = self.builtins.get(&ftd.name) {
                        exports.insert(ftd.name.clone(), fn_type.clone());
                    }
                }
                Statement::FunctionDeclaration(fd) if !privates.contains(fd.name.as_str()) => {
                    // Function signature resolved from @param/@return types.
                    // Unresolved param types fall back to Unknown rather
                    // than aborting — we'd rather produce a usable seed
                    // than drop the whole module because one param type
                    // doesn't resolve yet.
                    if let Ok(fn_type) = self.function_type_from_decl(fd) {
                        exports.insert(fd.name.clone(), fn_type);
                    }
                }
                Statement::ExternBlockDeclaration(ext) => {
                    for t in &ext.types {
                        if !privates.contains(t.name.as_str()) {
                            exports.insert(t.name.clone(), Type::Ptr(t.name.clone()));
                        }
                    }
                    for f in &ext.functions {
                        if privates.contains(f.name.as_str()) {
                            continue;
                        }
                        let mut params = Vec::new();
                        for p in &f.params {
                            let ty = if p.is_out {
                                Type::Unknown
                            } else {
                                self.resolve_type_node(&p.type_node)
                                    .unwrap_or(Type::Unknown)
                            };
                            params.push(FunctionParam {
                                name: p.name.clone(),
                                ty,
                                has_default: p.default_value.is_some(),
                                is_mutable: p.is_mutable,
                            });
                        }
                        let returns = if let Some(rt) = &f.return_type {
                            vec![self.resolve_type_node(rt).unwrap_or(Type::Unknown)]
                        } else {
                            vec![Type::Void]
                        };
                        let fn_name = format!("{}.{}", ext.library, f.name);
                        let fn_type = Type::Function(FunctionSig {
                            name: fn_name,
                            type_params: Vec::new(),
                            params,
                            returns,
                        });
                        exports.insert(f.name.clone(), fn_type);
                    }
                }
                _ => {}
            }
        }

        Ok(exports)
    }

    fn collect_module_exports(
        &self,
        statements: &[Statement],
        private_names: &[String],
        env: &Environment,
        exports: &mut HashMap<String, Type>,
    ) -> Result<(), CheckError> {
        let privates: HashSet<&str> = private_names.iter().map(|s| s.as_str()).collect();
        for stmt in statements {
            let name = match stmt {
                Statement::FunctionDeclaration(fd) if !privates.contains(fd.name.as_str()) => {
                    Some(&fd.name)
                }
                Statement::LetStatement(ls) if ls.is_private != Some(true) => {
                    ls.bindings.first().map(|b| &b.name)
                }
                Statement::VarStatement(vs) if vs.is_private != Some(true) => {
                    vs.bindings.first().map(|b| &b.name)
                }
                Statement::TypeDeclaration(td) if !privates.contains(td.name.as_str()) => {
                    Some(&td.name)
                }
                Statement::EnumDeclaration(ed) if !privates.contains(ed.name.as_str()) => {
                    Some(&ed.name)
                }
                Statement::FunctionTypeDefDeclaration(ftd)
                    if !privates.contains(ftd.name.as_str()) =>
                {
                    Some(&ftd.name)
                }
                _ => None,
            };
            if let Some(n) = name {
                if let Ok(binding) = env.get(n) {
                    exports.insert(n.clone(), binding.ty.clone());
                }
            }
        }
        Ok(())
    }

    fn check_top_level_statements(
        &mut self,
        statements: &[Statement],
        env: &mut Environment,
    ) -> Result<(), CheckError> {
        // Forward-declare: types, enums, extern types, and function signatures
        self.forward_declare_all(statements, env)?;

        // Check each statement, accumulating errors instead of stopping at
        // the first. This lets `fai check` surface every issue in a single
        // pass so agents/users can fix them together rather than running
        // the pipeline once per error.
        for stmt in statements {
            if let Err(e) = self.check_top_level_statement(stmt, env) {
                // Prevent cascade: if a let/var binding fails, install its
                // names with Type::Unknown so downstream statements don't
                // also fail with "unknown name 'x'" (noise that hides the
                // real error).
                install_failed_binding_placeholders(stmt, env);
                self.collected_errors.push(e);
            }
        }
        Ok(())
    }

    fn check_top_level_statement(
        &mut self,
        stmt: &Statement,
        env: &mut Environment,
    ) -> Result<(), CheckError> {
        match stmt {
            Statement::UseStatement(_) => Ok(()),
            Statement::LetStatement(ls) => {
                // forward_declare_module_bindings already handled this if
                // every binding name is in the env; skip to avoid the
                // duplicate-name error from a redundant define.
                if ls.bindings.iter().all(|b| env.get(&b.name).is_ok()) {
                    return Ok(());
                }
                self.check_binding_statement(&ls.bindings, &ls.value, env, false, "let")
                    .map_err(|e| self.attach_location(e, &ls.location))?;
                Ok(())
            }
            Statement::VarStatement(vs) => {
                if vs.bindings.iter().all(|b| env.get(&b.name).is_ok()) {
                    return Ok(());
                }
                self.check_binding_statement(&vs.bindings, &vs.value, env, true, "var")
                    .map_err(|e| self.attach_location(e, &vs.location))?;
                Ok(())
            }
            Statement::AssignmentStatement(a) => {
                self.check_assignment_stmt(a, env)
                    .map_err(|e| self.attach_location(e, &a.location))?;
                Ok(())
            }
            Statement::FunctionDeclaration(fd) => {
                self.check_function(fd, env)
                    .map_err(|e| self.attach_location(e, &fd.location))?;
                Ok(())
            }
            Statement::TypeDeclaration(_)
            | Statement::EnumDeclaration(_)
            | Statement::FunctionTypeDefDeclaration(_) => Ok(()),
            Statement::TestDeclaration(td) => {
                self.check_test_declaration(td, env)
                    .map_err(|e| self.attach_location(e, &td.location))?;
                Ok(())
            }
            _ => {
                self.check_statement(stmt, env)?;
                Ok(())
            }
        }
    }
}

/// When a let/var binding statement fails to check, still install the
/// declared names into the environment with Type::Unknown. Otherwise
/// every later statement that references the binding ("unknown name 'x'")
/// piles cascade errors on top of the real one, drowning the useful
/// diagnostic. Non-binding statements have nothing to install so they fall
/// through — the direct error is the only one a user needs.
pub(super) fn install_failed_binding_placeholders(stmt: &Statement, env: &mut Environment) {
    match stmt {
        Statement::LetStatement(ls) => {
            for b in &ls.bindings {
                // Ignore errors (e.g. duplicate name) — we're just trying to
                // reduce cascade noise; a real duplicate-name error from the
                // original check_binding_statement call is already recorded.
                let _ = env.define(&b.name, Type::Unknown, false);
            }
        }
        Statement::VarStatement(vs) => {
            for b in &vs.bindings {
                let _ = env.define(&b.name, Type::Unknown, true);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qualify_module_path_std() {
        let path = vec!["std".to_string(), "http".to_string(), "request".to_string()];
        let result = Checker::qualify_module_path(None, &path);
        assert_eq!(result, "std.http.request");
    }

    #[test]
    fn test_qualify_module_path_external() {
        let path = vec!["SQLite".to_string(), "query".to_string()];
        let result = Checker::qualify_module_path(None, &path);
        assert_eq!(result, "SQLite.query");
    }

    #[test]
    fn test_qualify_module_path_external_with_current() {
        let path = vec!["SQLite".to_string(), "query".to_string()];
        let result = Checker::qualify_module_path(Some("MyApp.core"), &path);
        assert_eq!(result, "SQLite.query");
    }

    #[test]
    fn test_qualify_module_path_local_in_package() {
        let path = vec!["utils".to_string()];
        let result = Checker::qualify_module_path(Some("MyApp.main"), &path);
        assert_eq!(result, "MyApp.utils");
    }

    #[test]
    fn test_qualify_module_path_local_no_package() {
        let path = vec!["utils".to_string()];
        let result = Checker::qualify_module_path(None, &path);
        assert_eq!(result, "utils");
    }

    #[test]
    fn test_qualify_module_path_lowercase_current() {
        // Lowercase "current" means not a package → just use the path as-is
        let path = vec!["utils".to_string()];
        let result = Checker::qualify_module_path(Some("main"), &path);
        assert_eq!(result, "utils");
    }

    #[test]
    fn test_toposort_modules_empty() {
        let c = Checker::new();
        let result = c.toposort_modules(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_toposort_modules_no_deps() {
        let c = Checker::new();
        let modules = vec![
            PreparedModule {
                name: "a".to_string(),
                statements: vec![],
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: None,
            },
            PreparedModule {
                name: "b".to_string(),
                statements: vec![],
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: None,
            },
        ];
        let result = c.toposort_modules(&modules);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_install_builtins_method_populates_env() {
        let c = Checker::new();
        let mut env = Environment::new();
        c.install_builtins(&mut env).unwrap();
        assert!(env.get("print").is_ok());
        assert!(env.get("assert").is_ok());
    }

    #[test]
    fn test_resolve_type_fields_installs_error_fields() {
        let mut c = Checker::new();
        c.resolve_type_fields().unwrap();
        let fields = c.type_fields.get("Error").expect("Error fields");
        assert!(fields.contains_key("message"));
        assert!(fields.contains_key("type"));
    }

    #[test]
    fn test_resolve_type_fields_installs_response_fields() {
        let mut c = Checker::new();
        c.resolve_type_fields().unwrap();
        let fields = c.type_fields.get("Response").expect("Response fields");
        assert!(fields.contains_key("status"));
        assert!(fields.contains_key("body"));
        assert!(fields.contains_key("headers"));
    }
}
