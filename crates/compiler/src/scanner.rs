use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const SPECIAL_FILES: &[(&str, &str)] = &[
    ("page.jsx", "page"),
    ("page.tsx", "page"),
    ("layout.jsx", "layout"),
    ("layout.tsx", "layout"),
    ("loading.jsx", "loading"),
    ("loading.tsx", "loading"),
    ("error.jsx", "error"),
    ("error.tsx", "error"),
    ("not-found.jsx", "not_found"),
    ("not-found.tsx", "not_found"),
    ("template.jsx", "template"),
    ("template.tsx", "template"),
    ("server.js", "server"),
    ("server.ts", "server"),
];

#[derive(Debug, Clone, Serialize)]
pub struct ScannedRoute {
    pub path: String,
    pub dir: PathBuf,
    pub files: HashMap<String, PathBuf>,
    pub layouts: Vec<PathBuf>,
    pub params: Vec<String>,
}

pub fn scan_routes(app_dir: &Path) -> Vec<ScannedRoute> {
    let mut routes = Vec::new();
    walk_dir(app_dir, app_dir, &[], &mut routes);
    routes
}

fn walk_dir(
    dir: &Path,
    app_dir: &Path,
    parent_layouts: &[PathBuf],
    routes: &mut Vec<ScannedRoute>,
) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|_| panic!("Failed to read directory: {}", dir.display()))
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut files: HashMap<String, PathBuf> = HashMap::new();
    let mut layouts = parent_layouts.to_vec();

    // Collect special files
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            for &(pattern, key) in SPECIAL_FILES {
                if name == pattern {
                    files.insert(key.to_string(), entry.path());
                }
            }
        }
    }

    // If this dir has a layout, add to chain
    if let Some(layout_path) = files.get("layout") {
        layouts.push(layout_path.clone());
    }

    // If this dir has a page, it's a route
    if files.contains_key("page") {
        let rel_path = dir
            .strip_prefix(app_dir)
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .to_string();
        let url_path = dir_to_url_path(&rel_path);
        let params = extract_params(&rel_path);

        routes.push(ScannedRoute {
            path: url_path,
            dir: dir.to_path_buf(),
            files,
            layouts: layouts.clone(),
            params,
        });
    }

    // Recurse into subdirectories
    for entry in &entries {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk_dir(&entry.path(), app_dir, &layouts, routes);
        }
    }
}

fn dir_to_url_path(rel_path: &str) -> String {
    if rel_path.is_empty() || rel_path == "." {
        return "/".to_string();
    }

    let segments: Vec<&str> = rel_path
        .split('/')
        .filter_map(|seg| {
            // Route groups: (name) -> ignored
            if seg.starts_with('(') && seg.ends_with(')') {
                return None;
            }
            // Catch-all: [...param] -> *
            if seg.starts_with("[...") && seg.ends_with(']') {
                return Some("*");
            }
            // Dynamic: [param] -> :param
            if seg.starts_with('[') && seg.ends_with(']') {
                let param = &seg[1..seg.len() - 1];
                // Leak is fine for static route paths
                return Some(Box::leak(format!(":{}", param).into_boxed_str()) as &str);
            }
            Some(seg)
        })
        .collect();

    format!("/{}", segments.join("/"))
}

fn extract_params(rel_path: &str) -> Vec<String> {
    if rel_path.is_empty() {
        return Vec::new();
    }

    rel_path
        .split('/')
        .filter(|seg| seg.starts_with('[') && seg.ends_with(']'))
        .map(|seg| {
            if seg.starts_with("[...") {
                seg[4..seg.len() - 1].to_string()
            } else {
                seg[1..seg.len() - 1].to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_fixture(base: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let full = base.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
        }
    }

    #[test]
    fn discovers_routes() {
        let dir = std::env::temp_dir().join(format!("appbase-scanner-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        setup_fixture(&dir, &[
            ("page.jsx", "export default function Home() {}"),
            ("layout.jsx", "export default function Layout({ children }) {}"),
            ("loading.jsx", "export default function Loading() {}"),
            ("error.jsx", "export default function Error() {}"),
            ("not-found.jsx", "export default function NotFound() {}"),
            ("about/page.jsx", "export default function About() {}"),
            ("todos/page.jsx", "export default function Todos() {}"),
            ("todos/layout.jsx", "export default function TodosLayout({ children }) {}"),
            ("todos/server.js", "export async function getTodos() {}"),
            ("todos/[id]/page.jsx", "export default function TodoDetail() {}"),
            ("todos/[id]/server.js", "export async function getTodo() {}"),
            ("settings/page.jsx", "export default function Settings() {}"),
            ("settings/profile/page.jsx", "export default function Profile() {}"),
            ("components/Header.jsx", "export default function Header() {}"),
            ("lib/utils.js", "export function format() {}"),
        ]);

        let routes = scan_routes(&dir);
        let mut paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        paths.sort();

        assert_eq!(paths, vec![
            "/",
            "/about",
            "/settings",
            "/settings/profile",
            "/todos",
            "/todos/:id",
        ]);

        // Check special files
        let root = routes.iter().find(|r| r.path == "/").unwrap();
        assert!(root.files.contains_key("page"));
        assert!(root.files.contains_key("layout"));
        assert!(root.files.contains_key("loading"));
        assert!(root.files.contains_key("error"));
        assert!(root.files.contains_key("not_found"));

        // Check server.js
        let todos = routes.iter().find(|r| r.path == "/todos").unwrap();
        assert!(todos.files.contains_key("server"));

        // Check dynamic params
        let todo_detail = routes.iter().find(|r| r.path == "/todos/:id").unwrap();
        assert_eq!(todo_detail.params, vec!["id"]);

        // Check layout hierarchy
        assert_eq!(todo_detail.layouts.len(), 2);

        // No routes for components/ or lib/
        assert!(!paths.contains(&"/components"));
        assert!(!paths.contains(&"/lib"));

        fs::remove_dir_all(&dir).unwrap();
    }
}
