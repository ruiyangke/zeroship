#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    #[test]
    fn shipped_addon_imports_the_node_api_and_exports_its_registrar() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate lives under the workspace crates directory");
        let build = Command::new(env!("CARGO"))
            .current_dir(workspace)
            .args([
                "build",
                "--locked",
                "-p",
                env!("CARGO_PKG_NAME"),
                "--lib",
                "--message-format=json",
            ])
            .output()
            .expect("run the shipped library build");
        require_success(&build, "shipped library build");

        let cdylib = emitted_cdylib(&build.stdout).expect("Cargo emitted no Linux cdylib artifact");
        let symbols = Command::new("nm")
            .arg("-D")
            .arg(&cdylib)
            .output()
            .expect("run nm against the shipped cdylib");
        require_success(&symbols, "dynamic symbol inspection");
        let symbols = String::from_utf8(symbols.stdout).expect("nm output is UTF-8");

        assert!(
            has_symbol(&symbols, "U", "napi_create_function"),
            "{} must leave napi_create_function undefined so the Node host resolves it; \
             dyn-symbols may only be enabled by the crate's dev-dependency",
            cdylib.display()
        );
        assert!(
            has_symbol(&symbols, "T", "napi_register_module_v1"),
            "{} must export napi_register_module_v1 so Node can register the addon",
            cdylib.display()
        );
    }

    fn emitted_cdylib(events: &[u8]) -> Option<PathBuf> {
        events
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .filter(|event| event["reason"] == "compiler-artifact")
            .filter(|event| {
                event["target"]["kind"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
            })
            .flat_map(|event| {
                event["filenames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .find(|path| path.extension().is_some_and(|extension| extension == "so"))
    }

    fn has_symbol(symbols: &str, kind: &str, name: &str) -> bool {
        symbols.lines().any(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            fields.len() >= 2
                && fields[fields.len() - 2] == kind
                && fields[fields.len() - 1] == name
        })
    }

    fn require_success(output: &Output, operation: &str) {
        assert!(
            output.status.success(),
            "{operation} failed with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
