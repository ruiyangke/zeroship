pub mod bundler;
pub mod scanner;
pub mod directory;

use serde::Serialize;
use std::collections::HashSet;
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
    compile_with_options(source, target, false)
}

pub fn compile_with_options(source: &str, target: Target, minify: bool) -> CompileResult {
    swc_core::common::GLOBALS.set(&Default::default(), || {
        compile_inner(source, target, minify)
    })
}

fn compile_inner(source: &str, target: Target, minify: bool) -> CompileResult {
    let cm: Lrc<SourceMap> = Default::default();

    let module = parse(source, &cm);
    let analysis = analyze(&module);

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
}

fn analyze(module: &Module) -> Analysis {
    let mut a = Analysis::default();

    // Check file-level "use server" directive
    if let Some(ModuleItem::Stmt(Stmt::Expr(ExprStmt { expr, .. }))) = module.body.first() {
        if let Expr::Lit(Lit::Str(s)) = &**expr {
            if s.value == "use server" {
                a.has_file_directive = true;
            }
        }
    }

    // Pass 1: Find taint sources (zeroship imports except 'serve')
    let mut taint_collector = TaintCollector {
        tainted: &mut a.tainted_bindings,
    };
    module.visit_with(&mut taint_collector);

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

// --- Pass 1: Collect taint sources ---

struct TaintCollector<'a> {
    tainted: &'a mut HashSet<String>,
}

impl Visit for TaintCollector<'_> {
    fn visit_import_decl(&mut self, import: &ImportDecl) {
        if import.src.value == "zeroship" {
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
                    if import.src.value == "zeroship" {
                        if self.target == Target::Node {
                            // Keep but remove 'serve'
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
                        // Rust target: drop all zeroship imports
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
                // Remove zeroship imports
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) => {
                    if import.src.value != "zeroship" {
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
}
