//! RPC surface discovery for a prepared target graph.
//!
//! `addRpcRoutes` is generated from the server target's reachable
//! modules, not just declarations that happen to live in the entry
//! file. This keeps fullstack projects free to put RPC endpoints in
//! normal app folders such as `data/tasks` or `auth`.

use crate::interface;
use crate::rpc_dispatch;
use fai_compiler::ast::{FunctionDeclaration, Statement, TypeDeclaration};
use fai_compiler::PreparedProgram;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub(crate) struct RpcFunction {
    pub module: Option<String>,
    pub name: String,
    pub key: String,
    pub declaration: FunctionDeclaration,
}

#[derive(Debug, Clone)]
pub(crate) struct RpcType {
    pub module: Option<String>,
    pub name: String,
    pub key: String,
    pub declaration: TypeDeclaration,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RpcSurface {
    pub functions: Vec<RpcFunction>,
    pub types: Vec<RpcType>,
}

impl RpcSurface {
    pub(crate) fn is_empty(&self) -> bool {
        self.functions.is_empty() && self.types.is_empty()
    }

    pub(crate) fn dispatch_functions(&self) -> Vec<rpc_dispatch::DispatchFunction> {
        self.functions
            .iter()
            .map(|f| rpc_dispatch::DispatchFunction {
                module: f.module.clone(),
                name: f.name.clone(),
                key: f.key.clone(),
                params: f
                    .declaration
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .collect(),
            })
            .collect()
    }

    pub(crate) fn to_schema(&self) -> interface::InterfaceSpec {
        let functions = self
            .functions
            .iter()
            .map(|f| interface::extract_function_with_origin(&f.declaration, f.module.as_deref()))
            .collect();
        let types = self
            .types
            .iter()
            .map(|t| interface::extract_type_with_origin(&t.declaration, t.module.as_deref()))
            .collect();
        interface::make_remote_schema(functions, types)
    }
}

pub(crate) fn collect_from_source(
    source: &str,
    source_root: Option<&str>,
    entry_path: Option<&str>,
) -> Result<RpcSurface, String> {
    let prepared = fai_compiler::prepare_source_with_synthetic_and_entry(
        source,
        source_root,
        Vec::new(),
        entry_path,
    )?;
    collect_from_prepared(&prepared)
}

pub(crate) fn collect_from_prepared(prepared: &PreparedProgram) -> Result<RpcSurface, String> {
    let mut surface = RpcSurface::default();
    let mut seen_functions = HashMap::<String, String>::new();
    let mut seen_types = HashMap::<String, String>::new();

    collect_statements(
        None,
        &prepared.serde_ast.statements,
        &mut surface,
        &mut seen_functions,
        &mut seen_types,
    )?;

    for module in &prepared.modules {
        collect_statements(
            Some(module.name.as_str()),
            &module.statements,
            &mut surface,
            &mut seen_functions,
            &mut seen_types,
        )?;
    }

    Ok(surface)
}

fn collect_statements(
    module: Option<&str>,
    statements: &[Statement],
    surface: &mut RpcSurface,
    seen_functions: &mut HashMap<String, String>,
    seen_types: &mut HashMap<String, String>,
) -> Result<(), String> {
    for stmt in statements {
        match stmt {
            Statement::FunctionDeclaration(fd) if should_expose_function(fd) => {
                let key = rpc_key(module, &fd.name);
                if let Some(prev) = seen_functions.insert(key.clone(), key.clone()) {
                    return Err(format!(
                        "duplicate remote function '{}' in target graph",
                        prev
                    ));
                }
                surface.functions.push(RpcFunction {
                    module: module.map(str::to_string),
                    name: fd.name.clone(),
                    key,
                    declaration: fd.clone(),
                });
            }
            Statement::TypeDeclaration(td) if should_expose_type(td) => {
                let key = rpc_key(module, &td.name);
                if let Some(prev) = seen_types.insert(key.clone(), key.clone()) {
                    return Err(format!("duplicate remote type '{}' in target graph", prev));
                }
                surface.types.push(RpcType {
                    module: module.map(str::to_string),
                    name: td.name.clone(),
                    key,
                    declaration: td.clone(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn should_expose_function(fd: &FunctionDeclaration) -> bool {
    fd.is_remote
        && fd.name != "main"
        && !fd.name.starts_with('<')
        && !fd.name.starts_with("__")
        && !fd.is_private.unwrap_or(false)
}

fn should_expose_type(td: &TypeDeclaration) -> bool {
    td.is_remote && !td.is_private.unwrap_or(false)
}

fn rpc_key(module: Option<&str>, name: &str) -> String {
    match module {
        Some(module) if !module.is_empty() => format!("{}.{}", module, name),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fai_rpc_surface_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn collects_remote_defs_from_imported_folder_modules() {
        let dir = temp_dir("imported");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("platforms/server")).unwrap();
        std::fs::create_dir_all(src.join("data/tasks")).unwrap();
        let entry_path = src.join("platforms/server/main.fai");
        let entry = concat!(
            "use { getTasks } from data.tasks\n\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  addRpcRoutes({})\n",
            "end\n",
        );
        std::fs::write(&entry_path, entry).unwrap();
        std::fs::write(
            src.join("data/tasks/main.fai"),
            concat!(
                "remote type Task\n",
                "  id Int\n",
                "end\n\n",
                "remote def getTasks\n",
                "    @return Task[]\n",
                "do\n",
                "  []\n",
                "end\n",
            ),
        )
        .unwrap();

        let surface = collect_from_source(
            entry,
            Some(src.to_str().unwrap()),
            Some(entry_path.to_str().unwrap()),
        )
        .unwrap();

        assert_eq!(surface.functions.len(), 1);
        assert_eq!(surface.functions[0].module.as_deref(), Some("data.tasks"));
        assert_eq!(surface.functions[0].name, "getTasks");
        assert_eq!(surface.functions[0].key, "data.tasks.getTasks");
        assert_eq!(surface.types.len(), 1);
        assert_eq!(surface.types[0].key, "data.tasks.Task");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_transitive_remote_defs_in_target_graph() {
        let dir = temp_dir("transitive");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("platforms/server")).unwrap();
        std::fs::create_dir_all(src.join("pages")).unwrap();
        std::fs::create_dir_all(src.join("data/tasks")).unwrap();
        let entry_path = src.join("platforms/server/main.fai");
        let entry = concat!(
            "use { HomePage } from pages\n\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  addRpcRoutes({})\n",
            "end\n",
        );
        std::fs::write(&entry_path, entry).unwrap();
        std::fs::write(
            src.join("pages/home.fai"),
            concat!(
                "use { getTasks } from data.tasks\n\n",
                "def HomePage\n",
                "    @return String\n",
                "do\n",
                "  'home'\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/tasks/main.fai"),
            concat!(
                "remote def getTasks\n",
                "    @return String[]\n",
                "do\n",
                "  []\n",
                "end\n",
            ),
        )
        .unwrap();

        let surface = collect_from_source(
            entry,
            Some(src.to_str().unwrap()),
            Some(entry_path.to_str().unwrap()),
        )
        .unwrap();

        assert_eq!(surface.functions.len(), 1);
        assert_eq!(surface.functions[0].key, "data.tasks.getTasks");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_remote_defs_outside_the_target_graph() {
        let dir = temp_dir("unreachable");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("platforms/server")).unwrap();
        std::fs::create_dir_all(src.join("data/tasks")).unwrap();
        std::fs::create_dir_all(src.join("data/unused")).unwrap();
        let entry_path = src.join("platforms/server/main.fai");
        let entry = concat!(
            "use { getTasks } from data.tasks\n\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  addRpcRoutes({})\n",
            "end\n",
        );
        std::fs::write(&entry_path, entry).unwrap();
        std::fs::write(
            src.join("data/tasks/main.fai"),
            concat!(
                "remote def getTasks\n",
                "    @return String[]\n",
                "do\n",
                "  []\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/unused/main.fai"),
            concat!(
                "remote def deleteEverything\n",
                "    @return Void\n",
                "do\n",
                "end\n",
            ),
        )
        .unwrap();

        let surface = collect_from_source(
            entry,
            Some(src.to_str().unwrap()),
            Some(entry_path.to_str().unwrap()),
        )
        .unwrap();

        let keys: Vec<&str> = surface.functions.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["data.tasks.getTasks"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
