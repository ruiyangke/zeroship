use crate::scanner::scan_routes;
use crate::{compile, Target};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct CompiledRoute {
    pub path: String,
    pub page: Option<String>,
    pub layout: Option<String>,
    pub loading: Option<String>,
    pub error: Option<String>,
    pub not_found: Option<String>,
    pub template: Option<String>,
    pub params: Vec<String>,
    pub layout_chain: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryCompileResult {
    pub routes: Vec<CompiledRoute>,
    pub server_bundle: String,
    pub server_functions: Vec<String>,
}

pub fn compile_directory(app_dir: &Path, target: Target) -> DirectoryCompileResult {
    let scanned = scan_routes(app_dir);

    let mut all_server_fns: Vec<String> = Vec::new();
    let mut server_chunks: Vec<String> = Vec::new();
    let mut compiled_routes: Vec<CompiledRoute> = Vec::new();
    let mut seen_fns: HashSet<String> = HashSet::new();

    for route in &scanned {
        let mut compiled = CompiledRoute {
            path: route.path.clone(),
            page: None,
            layout: None,
            loading: None,
            error: None,
            not_found: None,
            template: None,
            params: route.params.clone(),
            layout_chain: route.layouts.iter().map(|p| p.to_string_lossy().to_string()).collect(),
        };

        // Compile server.js (implicitly "use server")
        if let Some(server_path) = route.files.get("server") {
            let source = fs::read_to_string(server_path).unwrap();
            let server_source = if source.trim_start().starts_with("\"use server\"") {
                source
            } else {
                format!("\"use server\"\n{}", source)
            };
            let result = compile(&server_source, target);
            if !result.server.is_empty() {
                server_chunks.push(result.server);
            }
            for f in &result.server_functions {
                if seen_fns.insert(f.clone()) {
                    all_server_fns.push(f.clone());
                }
            }
        }

        // Compile page
        if let Some(page_path) = route.files.get("page") {
            let source = fs::read_to_string(page_path).unwrap();
            let result = compile(&source, target);
            compiled.page = Some(result.client);
            if !result.server.is_empty() {
                server_chunks.push(result.server);
            }
            for f in &result.server_functions {
                if seen_fns.insert(f.clone()) {
                    all_server_fns.push(f.clone());
                }
            }
        }

        // Read client-only files (no compilation needed)
        for (key, field_setter) in [
            ("layout", &mut compiled.layout),
            ("loading", &mut compiled.loading),
            ("error", &mut compiled.error),
            ("not_found", &mut compiled.not_found),
            ("template", &mut compiled.template),
        ] {
            if let Some(path) = route.files.get(key) {
                *field_setter = Some(fs::read_to_string(path).unwrap());
            }
        }

        compiled_routes.push(compiled);
    }

    let server_bundle = server_chunks.join("\n\n");

    DirectoryCompileResult {
        routes: compiled_routes,
        server_bundle,
        server_functions: all_server_fns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_fixture(base: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let full = base.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
        }
    }

    #[test]
    fn compiles_directory() {
        let dir = std::env::temp_dir().join(format!("zeroship-dircompile-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        setup_fixture(&dir, &[
            ("layout.jsx", "export default function Layout({ children }) { return <div>{children}</div> }"),
            ("page.jsx", "export default function Home() { return <h1>Home</h1> }"),
            ("todos/page.jsx", "export default function Todos() { return <div>Todos</div> }"),
            ("todos/server.js", r#"import { db } from 'zeroship'
const todos = db.collection('todos')
export async function getTodos() { return todos.find() }
export async function addTodo(text) { return todos.insert({ text, done: false }) }
"#),
        ]);

        let result = compile_directory(&dir, Target::Rust);

        // Check routes
        let paths: Vec<&str> = result.routes.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/"));
        assert!(paths.contains(&"/todos"));

        // Check server functions
        assert!(result.server_functions.contains(&"getTodos".to_string()));
        assert!(result.server_functions.contains(&"addTodo".to_string()));

        // Check server bundle
        assert!(result.server_bundle.contains("getTodos"));
        assert!(result.server_bundle.contains("addTodo"));
        assert!(result.server_bundle.contains("db.collection"));

        // Check page client code
        let root = result.routes.iter().find(|r| r.path == "/").unwrap();
        assert!(root.page.as_ref().unwrap().contains("Home"));
        let todos = result.routes.iter().find(|r| r.path == "/todos").unwrap();
        assert!(todos.page.as_ref().unwrap().contains("Todos"));

        // Check layout
        assert!(root.layout.is_some());

        fs::remove_dir_all(&dir).unwrap();
    }
}
