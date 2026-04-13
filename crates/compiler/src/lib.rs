pub mod bundler;
pub mod scanner;
pub mod directory;

use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use swc_core::common::{sync::Lrc, FileName, SourceMap};
use swc_core::ecma::ast::*;
use swc_core::ecma::codegen::{text_writer::JsWriter, Emitter};
use swc_core::ecma::parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_core::ecma::transforms::react::{react, Options as ReactOptions, Runtime};
use swc_core::ecma::visit::{VisitMut, VisitMutWith, Visit, VisitWith};

#[derive(Debug, Clone, Serialize)]
pub struct CompileResult {
    pub server: String,
    pub client: String,
    pub entry_component: Option<String>,
    pub server_functions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Node,
    Rust,
}

pub fn compile(source: &str, target: Target) -> CompileResult {
    compile_with_options(source, target, false, None)
}

/// Compile with a project root for module resolution.
/// When `project_root` is provided, the compiler resolves imports to check
/// for `"use server"` directives in dependencies.
pub fn compile_with_options(
    source: &str,
    target: Target,
    minify: bool,
    project_root: Option<&Path>,
) -> CompileResult {
    swc_core::common::GLOBALS.set(&Default::default(), || {
        compile_inner(source, target, minify, project_root)
    })
}

fn compile_inner(
    source: &str,
    target: Target,
    minify: bool,
    project_root: Option<&Path>,
) -> CompileResult {
    let cm: Lrc<SourceMap> = Default::default();

    let module = parse(source, &cm);
    let analysis = analyze(&module, project_root);

    let server_module = parse(source, &cm);
    let server = generate_server(server_module, &analysis, target, &cm, minify);

    let client_module = parse(source, &cm);
    let client = generate_client(client_module, &analysis, &cm, minify);

    CompileResult {
        server,
        client,
        entry_component: analysis.entry_component,
        server_functions: analysis.exported_server_fns,
    }
}

fn parse(source: &str, cm: &Lrc<SourceMap>) -> Module {
    let fm = cm.new_source_file(FileName::Anon.into(), source.to_string());
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        EsVersion::latest(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    parser.parse_module().expect("Failed to parse module")
}

fn emit_with_minify(module: &Module, cm: &Lrc<SourceMap>, minify: bool) -> String {
    let mut buf = Vec::new();
    let mut emitter = Emitter {
        cfg: swc_core::ecma::codegen::Config::default().with_minify(minify),
        comments: None,
        cm: cm.clone(),
        wr: Box::new(JsWriter::new(cm.clone(), "\n", &mut buf, None)),
    };
    emitter.emit_module(module).expect("Failed to emit module");
    String::from_utf8(buf).expect("Invalid UTF-8 in output")
}

// --- Analysis ---

#[derive(Debug, Default)]
struct Analysis {
    tainted_bindings: HashSet<String>,
    server_functions: HashSet<String>,
    exported_server_fns: Vec<String>,
    entry_component: Option<String>,
    has_file_directive: bool,
    /// Module specifiers that have `"use server"` at file level.
    server_modules: HashSet<String>,
}

fn analyze(module: &Module, project_root: Option<&Path>) -> Analysis {
    let mut a = Analysis::default();

    // Check file-level "use server" directive
    if let Some(ModuleItem::Stmt(Stmt::Expr(ExprStmt { expr, .. }))) = module.body.first() {
        if let Expr::Lit(Lit::Str(s)) = &**expr {
            if s.value == "use server" {
                a.has_file_directive = true;
            }
        }
    }

    // Resolve which imported modules have "use server" at file level
    let server_modules = resolve_server_modules(module, project_root);

    // Pass 1: Find taint sources — imports from "use server" modules
    let mut taint_collector = TaintCollector {
        tainted: &mut a.tainted_bindings,
        server_modules: &server_modules,
    };
    module.visit_with(&mut taint_collector);
    a.server_modules = server_modules;

    // Pass 2: Propagate taint to direct bindings
    let mut propagator = TaintPropagator {
        tainted: a.tainted_bindings.clone(),
        new_tainted: HashSet::new(),
    };
    module.visit_with(&mut propagator);
    a.tainted_bindings.extend(propagator.new_tainted);

    // Pass 3: Detect server functions
    let mut detector = ServerFnDetector {
        tainted: &a.tainted_bindings,
        server_fns: HashSet::new(),
        exported_server_fns: Vec::new(),
        has_file_directive: a.has_file_directive,
    };
    module.visit_with(&mut detector);
    a.server_functions = detector.server_fns;
    a.exported_server_fns = detector.exported_server_fns;

    // Find serve() call
    let mut serve_finder = ServeFinder { entry: None };
    module.visit_with(&mut serve_finder);
    a.entry_component = serve_finder.entry;

    a
}

// --- Module resolution: check if imported modules have "use server" ---

/// Resolve which imported modules have `"use server"` at file level.
/// Returns a set of module specifiers (e.g. "@zeroship/db", "./api") that are server modules.
fn resolve_server_modules(module: &Module, project_root: Option<&Path>) -> HashSet<String> {
    let mut server_modules = HashSet::new();

    for item in &module.body {
        if let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item {
            let src = import.src.value.as_str().unwrap_or_default().to_string();

            // Skip 'serve' source — it's the client-side render function
            if src == "zeroship" {
                continue;
            }

            // Try to resolve the module and check for "use server"
            if let Some(root) = project_root {
                if is_server_module(&src, root) {
                    server_modules.insert(src);
                }
            }
        }
    }

    server_modules
}

/// Check if a module has `"use server"` as its first statement.
/// Resolves the module path from `node_modules/` and reads the entry file.
fn is_server_module(specifier: &str, project_root: &Path) -> bool {
    let entry_path = resolve_module_entry(specifier, project_root);
    let Some(path) = entry_path else { return false };
    let Ok(source) = std::fs::read_to_string(&path) else { return false };

    // Check if first non-empty line is "use server" (as a JS string expression)
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("/*") {
            continue;
        }
        return trimmed == r#""use server""#
            || trimmed == r#""use server";"#
            || trimmed == "'use server'"
            || trimmed == "'use server';";
    }
    false
}

/// Resolve a module specifier to a file path.
/// Handles: bare specifiers (`@zeroship/db`) via node_modules lookup,
/// and relative paths (`./api`) relative to project root.
fn resolve_module_entry(specifier: &str, project_root: &Path) -> Option<PathBuf> {
    if specifier.starts_with('.') {
        // Relative import — resolve from project root
        let candidates = [
            format!("{specifier}.ts"),
            format!("{specifier}.tsx"),
            format!("{specifier}.js"),
            format!("{specifier}.jsx"),
            format!("{specifier}/index.ts"),
            format!("{specifier}/index.js"),
        ];
        for candidate in &candidates {
            let path = project_root.join(candidate);
            if path.exists() {
                return Some(path);
            }
        }
        return None;
    }

    // Bare specifier — look in node_modules
    let pkg_dir = project_root.join("node_modules").join(specifier);
    if !pkg_dir.exists() {
        return None;
    }

    // Read package.json for entry point
    let pkg_json = pkg_dir.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&pkg_json) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
            // Check "exports" → "." → "import" or "default"
            if let Some(exports) = parsed.get("exports") {
                if let Some(dot) = exports.get(".") {
                    for key in &["import", "default", "require"] {
                        if let Some(serde_json::Value::String(p)) = dot.get(key) {
                            let path = pkg_dir.join(p);
                            if path.exists() { return Some(path); }
                        }
                    }
                    // "." might be a direct string
                    if let Some(p) = dot.as_str() {
                        let path = pkg_dir.join(p);
                        if path.exists() { return Some(path); }
                    }
                }
            }

            // Fallback: "module" or "main" field
            for key in &["module", "main"] {
                if let Some(serde_json::Value::String(p)) = parsed.get(key) {
                    let path = pkg_dir.join(p);
                    if path.exists() { return Some(path); }
                }
            }
        }
    }

    // Fallback: common entry files
    for name in &["index.ts", "index.js", "src/index.ts", "src/index.js"] {
        let path = pkg_dir.join(name);
        if path.exists() { return Some(path); }
    }

    None
}

// --- Pass 1: Collect taint sources ---

struct TaintCollector<'a> {
    tainted: &'a mut HashSet<String>,
    server_modules: &'a HashSet<String>,
}

impl Visit for TaintCollector<'_> {
    fn visit_import_decl(&mut self, import: &ImportDecl) {
        let src = import.src.value.as_str().unwrap_or_default().to_string();

        // If this module was resolved as a "use server" module, taint all imports from it
        if self.server_modules.contains(&src) {
            for spec in &import.specifiers {
                match spec {
                    ImportSpecifier::Named(named) => {
                        self.tainted.insert(named.local.sym.to_string());
                    }
                    ImportSpecifier::Default(default) => {
                        self.tainted.insert(default.local.sym.to_string());
                    }
                    _ => {}
                }
            }
            return;
        }

        // Legacy: import { db, serve } from 'zeroship' — taint everything except 'serve'
        if src == "zeroship" {
            for spec in &import.specifiers {
                if let ImportSpecifier::Named(named) = spec {
                    let name = named.local.sym.to_string();
                    if name != "serve" {
                        self.tainted.insert(name);
                    }
                }
            }
        }
    }
}

// --- Pass 2: Propagate taint ---

struct TaintPropagator {
    tainted: HashSet<String>,
    new_tainted: HashSet<String>,
}

impl Visit for TaintPropagator {
    fn visit_var_declarator(&mut self, decl: &VarDeclarator) {
        if let Pat::Ident(ident) = &decl.name {
            if let Some(init) = &decl.init {
                let source = extract_source_ident(init);
                if let Some(name) = source {
                    if self.tainted.contains(&name) {
                        self.new_tainted.insert(ident.sym.to_string());
                    }
                }
            }
        }
    }
}

fn extract_source_ident(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Call(call) => match &call.callee {
            Callee::Expr(expr) => match &**expr {
                Expr::Ident(id) => Some(id.sym.to_string()),
                Expr::Member(member) => {
                    if let Expr::Ident(obj) = &*member.obj {
                        Some(obj.sym.to_string())
                    } else {
                        None
                    }
                }
                _ => None,
            },
            _ => None,
        },
        Expr::Member(member) => {
            if let Expr::Ident(obj) = &*member.obj {
                Some(obj.sym.to_string())
            } else {
                None
            }
        }
        Expr::Ident(id) => Some(id.sym.to_string()),
        _ => None,
    }
}

// --- Pass 3: Detect server functions ---

struct ServerFnDetector<'a> {
    tainted: &'a HashSet<String>,
    server_fns: HashSet<String>,
    exported_server_fns: Vec<String>,
    has_file_directive: bool,
}

impl Visit for ServerFnDetector<'_> {
    fn visit_module_item(&mut self, item: &ModuleItem) {
        match item {
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => {
                if let Decl::Fn(fn_decl) = &export.decl {
                    let name = fn_decl.ident.sym.to_string();
                    let has_directive = check_use_server_directive(&fn_decl.function);
                    let has_tainted_ref = check_tainted_refs(&fn_decl.function, self.tainted);
                    let is_file_level = self.has_file_directive;

                    if has_directive || has_tainted_ref || is_file_level {
                        self.server_fns.insert(name.clone());
                        if !self.exported_server_fns.contains(&name) {
                            self.exported_server_fns.push(name);
                        }
                    }
                }
            }
            ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))) => {
                let name = fn_decl.ident.sym.to_string();
                let has_directive = check_use_server_directive(&fn_decl.function);
                let has_tainted_ref = check_tainted_refs(&fn_decl.function, self.tainted);

                if has_directive || has_tainted_ref {
                    self.server_fns.insert(name.clone());
                    if has_directive && !self.exported_server_fns.contains(&name) {
                        self.exported_server_fns.push(name);
                    }
                }
            }
            _ => {}
        }
    }
}

fn check_use_server_directive(func: &Function) -> bool {
    if let Some(body) = &func.body {
        for stmt in &body.stmts {
            if let Stmt::Expr(ExprStmt { expr, .. }) = stmt {
                if let Expr::Lit(Lit::Str(s)) = &**expr {
                    if s.value == "use server" {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn check_tainted_refs(func: &Function, tainted: &HashSet<String>) -> bool {
    let mut checker = TaintedRefChecker {
        tainted,
        found: false,
    };
    func.visit_with(&mut checker);
    checker.found
}

struct TaintedRefChecker<'a> {
    tainted: &'a HashSet<String>,
    found: bool,
}

impl Visit for TaintedRefChecker<'_> {
    fn visit_ident(&mut self, ident: &Ident) {
        if self.tainted.contains(&ident.sym.to_string()) {
            self.found = true;
        }
    }
}

// --- serve() finder ---

struct ServeFinder {
    entry: Option<String>,
}

impl Visit for ServeFinder {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Callee::Expr(expr) = &call.callee {
            if let Expr::Ident(id) = &**expr {
                if id.sym == "serve" {
                    if let Some(ExprOrSpread { expr, .. }) = call.args.first() {
                        if let Expr::Ident(arg) = &**expr {
                            self.entry = Some(arg.sym.to_string());
                        }
                    }
                }
            }
        }
    }
}

// --- Server code generation ---

fn generate_server(mut module: Module, analysis: &Analysis, target: Target, cm: &Lrc<SourceMap>, minify: bool) -> String {
    if analysis.server_functions.is_empty() {
        return String::new();
    }

    let mut transformer = ServerTransformer {
        analysis,
        target,
    };
    module.visit_mut_with(&mut transformer);

    // Remove empty items
    module.body.retain(|item| !is_empty_item(item));

    let mut code = emit_with_minify(&module, cm, minify);

    // Append globalThis.__rpc for Rust target
    if target == Target::Rust && !analysis.exported_server_fns.is_empty() {
        code.push_str(&format!(
            "\n\nglobalThis.__rpc = {{ {} }}",
            analysis.exported_server_fns.join(", ")
        ));
    }

    code.trim().to_string()
}

struct ServerTransformer<'a> {
    analysis: &'a Analysis,
    target: Target,
}

impl VisitMut for ServerTransformer<'_> {
    fn visit_mut_module(&mut self, module: &mut Module) {
        // Remove file-level "use server" directive
        if self.analysis.has_file_directive {
            if let Some(ModuleItem::Stmt(Stmt::Expr(ExprStmt { expr, .. }))) = module.body.first() {
                if let Expr::Lit(Lit::Str(s)) = &**expr {
                    if s.value == "use server" {
                        module.body.remove(0);
                    }
                }
            }
        }

        // Process each item
        let mut keep = Vec::new();
        for item in module.body.drain(..) {
            match &item {
                // Imports
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                    let src = import.src.value.as_str().unwrap_or_default().to_string();
                    let is_server_module = src == "zeroship" || self.analysis.server_modules.contains(&src);
                    if is_server_module {
                        if self.target == Target::Node {
                            // Keep server imports but remove 'serve'
                            let mut import = import.clone();
                            import.specifiers.retain(|s| {
                                if let ImportSpecifier::Named(n) = s {
                                    n.local.sym != "serve"
                                } else {
                                    true
                                }
                            });
                            if !import.specifiers.is_empty() {
                                keep.push(ModuleItem::ModuleDecl(ModuleDecl::Import(import)));
                            }
                        }
                        // Rust target: drop all server imports (primitives are on globalThis)
                    } else {
                        keep.push(item);
                    }
                }

                // Exported functions
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => {
                    if let Decl::Fn(fn_decl) = &export.decl {
                        if self.analysis.server_functions.contains(&fn_decl.ident.sym.to_string()) {
                            if self.target == Target::Rust {
                                // Strip export, keep function
                                let mut fn_decl = fn_decl.clone();
                                strip_use_server(&mut fn_decl.function);
                                keep.push(ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))));
                            } else {
                                let mut export = export.clone();
                                if let Decl::Fn(fn_decl) = &mut export.decl {
                                    strip_use_server(&mut fn_decl.function);
                                }
                                keep.push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)));
                            }
                        }
                    }
                }

                // Non-exported functions
                ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))) => {
                    if self.analysis.server_functions.contains(&fn_decl.ident.sym.to_string()) {
                        let mut fn_decl = fn_decl.clone();
                        strip_use_server(&mut fn_decl.function);
                        keep.push(ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))));
                    }
                }

                // Variable declarations — keep only tainted bindings
                ModuleItem::Stmt(Stmt::Decl(Decl::Var(var_decl))) => {
                    let any_tainted = var_decl.decls.iter().any(|d| {
                        if let Pat::Ident(id) = &d.name {
                            self.analysis.tainted_bindings.contains(&id.sym.to_string())
                        } else {
                            false
                        }
                    });
                    if any_tainted {
                        keep.push(item);
                    }
                }

                // Drop everything else (components, styles, serve() calls, TS types)
                _ => {}
            }
        }
        module.body = keep;
    }
}

fn strip_use_server(func: &mut Function) {
    if let Some(body) = &mut func.body {
        body.stmts.retain(|stmt| {
            if let Stmt::Expr(ExprStmt { expr, .. }) = stmt {
                if let Expr::Lit(Lit::Str(s)) = &**expr {
                    return s.value != "use server";
                }
            }
            true
        });
    }
}

// --- Client code generation ---

fn generate_client(mut module: Module, analysis: &Analysis, cm: &Lrc<SourceMap>, minify: bool) -> String {
    let mut transformer = ClientTransformer { analysis };
    module.visit_mut_with(&mut transformer);
    module.body.retain(|item| !is_empty_item(item));

    // Transform JSX to React.createElement calls
    let unresolved = swc_core::common::Mark::new();
    let top_level = swc_core::common::Mark::new();
    let jsx_transform = react::<swc_core::common::comments::SingleThreadedComments>(
        cm.clone(),
        None,
        ReactOptions {
            runtime: Some(Runtime::Classic),
            ..Default::default()
        },
        top_level,
        unresolved,
    );
    let mut program = Program::Module(module);
    program.mutate(jsx_transform);
    let module = match program {
        Program::Module(m) => m,
        _ => unreachable!(),
    };

    emit_with_minify(&module, cm, minify)
}

struct ClientTransformer<'a> {
    analysis: &'a Analysis,
}

impl VisitMut for ClientTransformer<'_> {
    fn visit_mut_module(&mut self, module: &mut Module) {
        // Remove file-level "use server"
        if self.analysis.has_file_directive {
            if let Some(ModuleItem::Stmt(Stmt::Expr(ExprStmt { expr, .. }))) = module.body.first() {
                if let Expr::Lit(Lit::Str(s)) = &**expr {
                    if s.value == "use server" {
                        module.body.remove(0);
                    }
                }
            }
        }

        let mut keep = Vec::new();
        for item in module.body.drain(..) {
            match &item {
                // Remove server module imports from client
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                    let src = import.src.value.as_str().unwrap_or_default().to_string();
                    let is_server = src == "zeroship" || self.analysis.server_modules.contains(&src);
                    if !is_server {
                        keep.push(item);
                    }
                }

                // Exported functions — replace server fns with RPC stubs
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => {
                    if let Decl::Fn(fn_decl) = &export.decl {
                        let name = fn_decl.ident.sym.to_string();
                        if self.analysis.exported_server_fns.contains(&name) {
                            let params: Vec<String> = fn_decl.function.params.iter().map(|p| {
                                if let Pat::Ident(id) = &p.pat {
                                    id.sym.to_string()
                                } else {
                                    "_".to_string()
                                }
                            }).collect();
                            let stub = build_rpc_stub(&name, &params);
                            keep.push(stub);
                        } else {
                            keep.push(item);
                        }
                    } else {
                        keep.push(item);
                    }
                }

                // Non-exported server functions — remove from client
                ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))) => {
                    let name = fn_decl.ident.sym.to_string();
                    if self.analysis.server_functions.contains(&name) {
                        // If it's an exported server fn somehow not caught above, make stub
                        // Otherwise, just remove (it's a server helper)
                    } else {
                        // Strip "use server" from remaining functions
                        let mut fn_decl = fn_decl.clone();
                        strip_use_server(&mut fn_decl.function);
                        keep.push(ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))));
                    }
                }

                // Remove tainted variable declarations
                ModuleItem::Stmt(Stmt::Decl(Decl::Var(var_decl))) => {
                    let any_tainted = var_decl.decls.iter().any(|d| {
                        if let Pat::Ident(id) = &d.name {
                            self.analysis.tainted_bindings.contains(&id.sym.to_string())
                        } else {
                            false
                        }
                    });
                    if !any_tainted {
                        keep.push(item);
                    }
                }

                // Remove serve() calls
                ModuleItem::Stmt(Stmt::Expr(ExprStmt { expr, .. })) => {
                    let is_serve = if let Expr::Call(call) = &**expr {
                        if let Callee::Expr(callee) = &call.callee {
                            if let Expr::Ident(id) = &**callee {
                                id.sym == "serve"
                            } else { false }
                        } else { false }
                    } else { false };

                    if !is_serve {
                        keep.push(item);
                    }
                }

                // Keep everything else
                _ => keep.push(item),
            }
        }
        module.body = keep;
    }
}

fn build_rpc_stub(name: &str, params: &[String]) -> ModuleItem {
    let param_list = params.join(", ");
    let param_array = if params.is_empty() {
        String::new()
    } else {
        params.join(", ")
    };

    let stub_code = format!(
        r#"async function {name}({param_list}) {{
  const res = await fetch("/rpc", {{
    method: "POST",
    headers: {{ "Content-Type": "application/json" }},
    body: JSON.stringify({{
      jsonrpc: "2.0",
      method: "{name}",
      params: [{param_array}],
      id: Date.now()
    }})
  }});
  const data = await res.json();
  if (data.error) throw new Error(data.error.message);
  return data.result;
}}"#
    );

    let cm: Lrc<SourceMap> = Default::default();
    let stub_module = parse(&stub_code, &cm);
    stub_module.body.into_iter().next().unwrap()
}

fn is_empty_item(_item: &ModuleItem) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_split() {
        let source = r#"
import { db, serve } from 'zeroship'
const todos = db.collection('todos')
export async function addTodo(text) { return todos.insert({ text, done: false }) }
export async function getTodos() { return todos.find() }
function App() { return <div>hello</div> }
const styles = { wrapper: { color: 'red' } }
serve(App)
"#;
        let result = compile(source, Target::Node);
        assert!(result.server.contains("addTodo"));
        assert!(result.server.contains("getTodos"));
        assert!(result.server.contains("db.collection"));
        assert!(!result.server.contains("App"));
        assert!(!result.server.contains("styles"));

        assert!(result.client.contains("App"));
        assert!(result.client.contains("styles"));
        assert!(result.client.contains("fetch"));
        assert!(!result.client.contains("db.collection"));
        assert!(!result.client.contains("serve(App)"));

        assert_eq!(result.entry_component, Some("App".to_string()));
        assert_eq!(result.server_functions, vec!["addTodo", "getTodos"]);
    }

    #[test]
    fn use_server_directive() {
        let source = r#"
import { serve } from 'zeroship'
export async function getTime() {
  "use server"
  return Date.now()
}
function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile(source, Target::Node);
        assert!(result.server.contains("getTime"));
        assert!(result.server.contains("Date.now()"));
        assert!(!result.server.contains("use server"));
        assert_eq!(result.server_functions, vec!["getTime"]);
    }

    #[test]
    fn rust_target() {
        let source = r#"
import { db, serve } from 'zeroship'
const todos = db.collection('todos')
export async function addTodo(text) { return todos.insert({ text }) }
export async function getTodos() { return todos.find() }
function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile(source, Target::Rust);
        assert!(!result.server.contains("import"));
        assert!(!result.server.contains("export"));
        assert!(result.server.contains("globalThis.__rpc"));
        assert!(result.server.contains("addTodo"));
    }

    #[test]
    fn no_transitive_taint() {
        let source = r#"
import { db, serve } from 'zeroship'
const todos = db.collection('todos')
export async function getTodos() { return todos.find() }
export async function getActive() {
  const all = await getTodos()
  return all.filter(t => !t.done)
}
function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile(source, Target::Node);
        assert!(result.server.contains("getTodos"));
        assert!(!result.server.contains("getActive"));
        assert!(result.client.contains("getActive"));
    }

    #[test]
    fn typescript_support() {
        let source = r#"
import { db, serve } from 'zeroship'
interface Todo { id: string; text: string; done: boolean }
const todos = db.collection('todos')
export async function addTodo(text: string): Promise<Todo> {
  return todos.insert({ text, done: false })
}
function App(): JSX.Element { return <div>hello</div> }
serve(App)
"#;
        let result = compile(source, Target::Node);
        assert!(result.server.contains("addTodo"));
        assert!(result.client.contains("App"));
    }

    #[test]
    fn no_server_functions() {
        let source = r#"
import { serve } from 'zeroship'
function App() { return <div>hello</div> }
serve(App)
"#;
        let result = compile(source, Target::Node);
        assert!(result.server.is_empty());
        assert_eq!(result.server_functions.len(), 0);
        assert!(result.client.contains("App"));
    }

    // --- SDK tests: "use server" module resolution ---

    /// Create a temp project dir with a mock `@zeroship/db` package
    /// that has `"use server"` as its first line.
    fn setup_mock_project() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zeroship-compiler-test-{}-{}", std::process::id(), id));
        let pkg_dir = dir.join("node_modules/@zeroship/db");
        std::fs::create_dir_all(&pkg_dir).unwrap();

        // package.json
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"@zeroship/db","main":"src/index.ts"}"#,
        ).unwrap();

        // src/index.ts with "use server"
        std::fs::create_dir_all(pkg_dir.join("src")).unwrap();
        std::fs::write(
            pkg_dir.join("src/index.ts"),
            "\"use server\"\nexport function model() {}\nexport const t = {};\n",
        ).unwrap();

        dir
    }

    /// Create a mock `@zeroship/auth` package with "use server"
    fn add_mock_auth(dir: &std::path::Path) {
        let pkg_dir = dir.join("node_modules/@zeroship/auth");
        std::fs::create_dir_all(pkg_dir.join("src")).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"@zeroship/auth","main":"src/index.ts"}"#,
        ).unwrap();
        std::fs::write(
            pkg_dir.join("src/index.ts"),
            "\"use server\"\nexport function hash() {}\nexport function verify() {}\n",
        ).unwrap();
    }

    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sdk_import_basic_split() {
        let dir = setup_mock_project();
        let source = r#"
import { model } from "@zeroship/db"
import { serve } from 'zeroship'

const todos = model("todos", { text: String, done: Boolean })

export async function addTodo(text) { return todos.create({ text }) }
export async function getTodos() { return todos.find({}) }

function App() { return <div>hello</div> }
serve(App)
"#;
        let result = compile_with_options(source, Target::Node, false, Some(&dir));

        assert!(result.server.contains("addTodo"));
        assert!(result.server.contains("getTodos"));
        assert!(result.server.contains("model"));
        assert!(!result.server.contains("App"));

        assert!(result.client.contains("App"));
        assert!(result.client.contains("fetch"));
        assert!(!result.client.contains("@zeroship/db"));

        assert_eq!(result.server_functions, vec!["addTodo", "getTodos"]);
        cleanup(&dir);
    }

    #[test]
    fn sdk_import_rust_target() {
        let dir = setup_mock_project();
        let source = r#"
import { model } from "@zeroship/db"
import { serve } from 'zeroship'

const todos = model("todos", { text: String })

export async function addTodo(text) { return todos.create({ text }) }
export async function getTodos() { return todos.find({}) }

function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile_with_options(source, Target::Rust, false, Some(&dir));

        assert!(!result.server.contains("import"));
        assert!(!result.server.contains("export"));
        assert!(result.server.contains("globalThis.__rpc"));
        assert!(result.server.contains("addTodo"));
        cleanup(&dir);
    }

    #[test]
    fn sdk_import_taint_propagation() {
        let dir = setup_mock_project();
        let source = r#"
import { model } from "@zeroship/db"
import { serve } from 'zeroship'

const users = model("users", { name: String })

export async function createUser(name) { return users.create({ name }) }
export async function getCount() { return users.countDocuments({}) }

export function formatName(name) { return name.trim().toLowerCase() }

function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile_with_options(source, Target::Node, false, Some(&dir));

        assert!(result.server.contains("createUser"));
        assert!(result.server.contains("getCount"));
        assert!(!result.server.contains("formatName"));
        assert!(result.client.contains("formatName"));
        cleanup(&dir);
    }

    #[test]
    fn sdk_import_multiple_packages() {
        let dir = setup_mock_project();
        add_mock_auth(&dir);
        let source = r#"
import { model } from "@zeroship/db"
import { hash } from "@zeroship/auth"
import { serve } from 'zeroship'

const users = model("users", { name: String, password: String })

export async function register(name, password) {
  const hashed = hash(password)
  return users.create({ name, password: hashed })
}

function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile_with_options(source, Target::Node, false, Some(&dir));

        assert!(result.server.contains("register"));
        assert!(!result.server.contains("App"));
        assert!(result.client.contains("App"));
        assert!(result.client.contains("fetch"));
        cleanup(&dir);
    }

    #[test]
    fn sdk_import_with_use_server() {
        let dir = setup_mock_project();
        let source = r#"
import { model } from "@zeroship/db"
import { serve } from 'zeroship'

const users = model("users", { name: String })

export async function getUsers() { return users.find({}) }

export async function getTime() {
  "use server"
  return Date.now()
}

function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile_with_options(source, Target::Node, false, Some(&dir));

        assert!(result.server.contains("getUsers"));
        assert!(result.server.contains("getTime"));
        assert!(result.server.contains("Date.now()"));
        assert!(!result.server.contains("use server"));
        assert_eq!(result.server_functions.len(), 2);
        cleanup(&dir);
    }

    #[test]
    fn non_server_module_not_tainted() {
        // Module without "use server" should not taint imports
        let dir = std::env::temp_dir().join(format!("zeroship-compiler-test-ns-{}", std::process::id()));
        let pkg_dir = dir.join("node_modules/lodash");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"lodash","main":"index.js"}"#,
        ).unwrap();
        std::fs::write(
            pkg_dir.join("index.js"),
            "export function debounce() {}\n",
        ).unwrap();

        let source = r#"
import { debounce } from "lodash"
import { serve } from 'zeroship'

const handler = debounce(() => {}, 100)

export function handleClick() { handler() }

function App() { return <div>hi</div> }
serve(App)
"#;
        let result = compile_with_options(source, Target::Node, false, Some(&dir));

        // handleClick uses debounce (NOT tainted) → should be client, not server
        assert!(!result.server.contains("handleClick"));
        assert!(result.client.contains("handleClick"));
        cleanup(&dir);
    }
}
