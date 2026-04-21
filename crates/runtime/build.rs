use std::path::PathBuf;

fn find_v8_include_dir() -> Option<PathBuf> {
    // Walk Cargo's dependency graph to find the v8 crate's source directory.
    // Its vendored V8 headers live at `<crate-src>/v8/include/`.
    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .ok()?;
    let v8_pkg = metadata.packages.iter().find(|p| p.name.as_str() == "v8")?;
    let v8_src = v8_pkg.manifest_path.parent()?;
    let include_dir = v8_src.join("v8/include");
    if include_dir.exists() {
        Some(include_dir.into())
    } else {
        None
    }
}

fn main() {
    println!("cargo:rerun-if-changed=native/zs_ext.cc");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file("native/zs_ext.cc")
        .std("c++20");

    if let Some(v8_include) = find_v8_include_dir() {
        println!("cargo:rerun-if-changed={}", v8_include.display());
        build.include(&v8_include);
        println!("cargo:warning=zs_ext: using V8 headers at {}", v8_include.display());
    } else {
        println!("cargo:warning=zs_ext: V8 include dir not found — zs_v8_* functions disabled");
    }

    build.compile("zs_ext");
}
