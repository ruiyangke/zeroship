use appbase_compiler::{compile, Target};
use std::fs;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let input = args.get(1).expect("Usage: appbase-compile <file> [--target=rust|node]");
    let target = args.iter()
        .find(|a| a.starts_with("--target="))
        .map(|a| a.strip_prefix("--target=").unwrap())
        .unwrap_or("node");

    let target = match target {
        "rust" => Target::Rust,
        "node" => Target::Node,
        _ => {
            eprintln!("Unknown target: {}. Use 'rust' or 'node'.", target);
            std::process::exit(1);
        }
    };

    let source = fs::read_to_string(input).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {}", input, e);
        std::process::exit(1);
    });

    let result = compile(&source, target);

    println!("{}", serde_json::to_string_pretty(&result).unwrap());
}
