//! `.appbundle` binary format — read/write with lazy decompression.
//!
//! See `docs/specs/appbundle-format.md` for the full specification.

use sha2::{Sha256, Digest};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A pre-resolved module to be loaded into V8.
#[derive(Debug, Clone)]
pub struct ModuleEntry {
    pub specifier: String,
    pub source: String,
}

/// In-memory representation of an `.appbundle` file.
pub struct AppBundle {
    /// Module index — O(1) lookup by specifier.
    modules: HashMap<String, BundleModule>,
    /// Ordered specifiers (insertion order matters — first = entry).
    order: Vec<String>,
    /// The entry module specifier (first in the index).
    entry: String,
    /// Raw data section bytes (modules read from here on demand).
    data: Vec<u8>,
}

/// Index entry for one module.
struct BundleModule {
    module_type: ModuleType,
    /// Offset into `data` for the compressed source.
    data_offset: u32,
    /// Size of the zstd-compressed source.
    compressed_size: u32,
    /// Size of the original (uncompressed) source.
    original_size: u32,
    /// Source (UTF-8 string) — only populated on first access.
    source: String,
    /// Whether `source` has been decompressed from the data section.
    decompressed: bool,
}

/// Module type tag stored in the index.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleType {
    EsModule = 0,
    Json = 1,
    Text = 2,
    Data = 3,
}

/// Metadata about a single module (no decompression needed).
#[derive(Debug, Clone)]
pub struct ModuleInfo {
    pub specifier: String,
    pub module_type: ModuleType,
    pub compressed_size: u32,
    pub original_size: u32,
}

/// Errors returned by bundle operations.
#[derive(Debug)]
pub enum BundleError {
    /// File too short to contain a valid header.
    TooShort,
    /// Magic bytes are not `APPB`.
    BadMagic,
    /// Unsupported format version.
    UnsupportedVersion(u32),
    /// SHA-256 integrity check failed.
    HashMismatch,
    /// Index entry contains invalid data.
    InvalidIndex(String),
    /// Bundle contains zero modules.
    EmptyBundle,
    /// Decompression failed.
    DecompressError(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "file too short for appbundle header"),
            Self::BadMagic => write!(f, "bad magic — expected APPB"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported version {v}"),
            Self::HashMismatch => write!(f, "SHA-256 hash mismatch"),
            Self::InvalidIndex(msg) => write!(f, "invalid index: {msg}"),
            Self::EmptyBundle => write!(f, "bundle contains zero modules"),
            Self::DecompressError(msg) => write!(f, "decompression error: {msg}"),
        }
    }
}

impl std::error::Error for BundleError {}

impl std::fmt::Display for ModuleType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EsModule => write!(f, "ES module"),
            Self::Json => write!(f, "JSON"),
            Self::Text => write!(f, "Text"),
            Self::Data => write!(f, "Data"),
        }
    }
}

impl ModuleType {
    fn from_u8(v: u8) -> Result<Self, BundleError> {
        match v {
            0 => Ok(Self::EsModule),
            1 => Ok(Self::Json),
            2 => Ok(Self::Text),
            3 => Ok(Self::Data),
            _ => Err(BundleError::InvalidIndex(format!("unknown module type {v}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"APPB";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 40; // 4 magic + 4 version + 32 hash
const ZSTD_LEVEL: i32 = 3;

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

impl AppBundle {
    /// Parse an `.appbundle` from raw bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BundleError> {
        if bytes.len() < HEADER_SIZE + 4 {
            return Err(BundleError::TooShort);
        }

        // 1. Magic
        if &bytes[0..4] != MAGIC {
            return Err(BundleError::BadMagic);
        }

        // 2. Version
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != VERSION {
            return Err(BundleError::UnsupportedVersion(version));
        }

        // 3. Expected hash
        let expected_hash: [u8; 32] = bytes[8..40].try_into().unwrap();

        // 4. Verify SHA-256 of bytes 40 → EOF
        let actual_hash: [u8; 32] = Sha256::digest(&bytes[40..]).into();
        if actual_hash != expected_hash {
            return Err(BundleError::HashMismatch);
        }

        // 5. Module count
        let module_count = read_u32(bytes, 40)?;
        if module_count == 0 {
            return Err(BundleError::EmptyBundle);
        }

        // 6. Read index entries
        let mut pos = 44;
        let mut modules = HashMap::with_capacity(module_count as usize);
        let mut order = Vec::with_capacity(module_count as usize);

        for _ in 0..module_count {
            // specifier_len: u16 LE
            if pos + 2 > bytes.len() {
                return Err(BundleError::TooShort);
            }
            let spec_len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;

            // specifier: UTF-8
            if pos + spec_len > bytes.len() {
                return Err(BundleError::TooShort);
            }
            let specifier = std::str::from_utf8(&bytes[pos..pos + spec_len])
                .map_err(|e| BundleError::InvalidIndex(format!("bad UTF-8 specifier: {e}")))?
                .to_string();
            pos += spec_len;

            // module_type: u8
            if pos + 1 > bytes.len() {
                return Err(BundleError::TooShort);
            }
            let module_type = ModuleType::from_u8(bytes[pos])?;
            pos += 1;

            // data_offset, compressed_size, original_size: u32 LE each (15 fixed bytes total)
            if pos + 12 > bytes.len() {
                return Err(BundleError::TooShort);
            }
            let data_offset = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let compressed_size = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let original_size = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;

            order.push(specifier.clone());
            modules.insert(specifier, BundleModule {
                module_type,
                data_offset,
                compressed_size,
                original_size,
                source: String::new(),
                decompressed: false,
            });
        }

        // 7-8. Data section = everything after the index
        let data = bytes[pos..].to_vec();
        let entry = order[0].clone();

        Ok(Self { modules, order, entry, data })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, BundleError> {
    if offset + 4 > bytes.len() {
        return Err(BundleError::TooShort);
    }
    Ok(u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()))
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

impl AppBundle {
    /// Serialize the bundle to the `.appbundle` binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        // Pass 1: compress all sources.
        struct Compressed {
            specifier: String,
            module_type: ModuleType,
            source_compressed: Vec<u8>,
            original_size: u32,
        }

        let mut entries: Vec<Compressed> = Vec::with_capacity(self.order.len());
        for spec in &self.order {
            let m = &self.modules[spec];
            let source_bytes = m.source.as_bytes();
            let source_compressed = zstd::encode_all(source_bytes, ZSTD_LEVEL).unwrap();

            entries.push(Compressed {
                specifier: spec.clone(),
                module_type: m.module_type,
                source_compressed,
                original_size: source_bytes.len() as u32,
            });
        }

        // Pass 2: compute data offsets and write.
        let mut data_offsets = Vec::with_capacity(entries.len());
        let mut offset: u32 = 0;
        for e in &entries {
            data_offsets.push(offset);
            offset += e.source_compressed.len() as u32;
        }

        // Pre-calculate total size for the output buffer.
        let index_size: usize = 4 + entries.iter().map(|e| {
            2 + e.specifier.len() + 1 + 12 // spec_len(2) + spec + type(1) + 3*u32(12)
        }).sum::<usize>();
        let data_size = offset as usize;
        let total = HEADER_SIZE + index_size + data_size;

        let mut buf = Vec::with_capacity(total);

        // Header: magic + version + 32 zero bytes (hash placeholder)
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&[0u8; 32]);

        // Module count
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());

        // Index entries
        for (i, e) in entries.iter().enumerate() {
            buf.extend_from_slice(&(e.specifier.len() as u16).to_le_bytes());
            buf.extend_from_slice(e.specifier.as_bytes());
            buf.push(e.module_type as u8);
            buf.extend_from_slice(&data_offsets[i].to_le_bytes());
            buf.extend_from_slice(&(e.source_compressed.len() as u32).to_le_bytes());
            buf.extend_from_slice(&e.original_size.to_le_bytes());
        }

        // Data section: concatenated compressed sources.
        for e in &entries {
            buf.extend_from_slice(&e.source_compressed);
        }

        // Compute SHA-256 of bytes 40 → end, write into bytes 8..40
        let hash: [u8; 32] = Sha256::digest(&buf[40..]).into();
        buf[8..40].copy_from_slice(&hash);

        buf
    }
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

impl AppBundle {
    /// Create a bundle from in-memory modules.
    ///
    /// Each tuple: `(specifier, module_type, source)`.
    /// The first entry becomes the entry module.
    pub fn new(entry: &str, modules: Vec<(String, ModuleType, String)>) -> Self {
        let mut map = HashMap::with_capacity(modules.len());
        let mut order = Vec::with_capacity(modules.len());

        for (specifier, module_type, source) in modules {
            order.push(specifier.clone());
            map.insert(specifier, BundleModule {
                module_type,
                data_offset: 0,
                compressed_size: 0,
                original_size: source.len() as u32,
                source,
                decompressed: true, // already in memory
            });
        }

        Self {
            modules: map,
            order,
            entry: entry.to_string(),
            data: Vec::new(),
        }
    }

    /// Returns the entry module specifier.
    pub fn entry(&self) -> &str {
        &self.entry
    }

    /// Returns the number of modules in the bundle.
    pub fn module_count(&self) -> usize {
        self.order.len()
    }

    /// Returns ordered module specifiers.
    pub fn module_names(&self) -> &[String] {
        &self.order
    }

    /// Returns metadata for a module without decompressing it.
    pub fn module_info(&self, specifier: &str) -> Option<ModuleInfo> {
        let m = self.modules.get(specifier)?;
        Some(ModuleInfo {
            specifier: specifier.to_string(),
            module_type: m.module_type,
            compressed_size: m.compressed_size,
            original_size: m.original_size,
        })
    }

    /// Returns metadata for all modules in order, without decompression.
    pub fn all_module_info(&self) -> Vec<ModuleInfo> {
        self.order.iter().filter_map(|s| self.module_info(s)).collect()
    }

    /// Lazily decompress and return the source for `specifier`.
    pub fn get_source(&mut self, specifier: &str) -> Option<&str> {
        // Two-phase borrow: check existence, then mutate.
        if !self.modules.contains_key(specifier) {
            return None;
        }
        let m = self.modules.get_mut(specifier).unwrap();
        if !m.decompressed {
            let start = m.data_offset as usize;
            let end = start + m.compressed_size as usize;
            if end > self.data.len() {
                return None;
            }
            let compressed = &self.data[start..end];
            let decompressed = zstd::decode_all(compressed).ok()?;
            m.source = String::from_utf8(decompressed).ok()?;
            m.decompressed = true;
        }
        // Re-borrow as immutable.
        Some(&self.modules[specifier].source)
    }

    /// Decompress ALL modules and return as `Vec<ModuleEntry>` for `load_modules()`.
    /// Entry module is first.
    pub fn to_module_entries(&mut self) -> Vec<ModuleEntry> {
        let mut result = Vec::with_capacity(self.order.len());
        let order = self.order.clone();
        for specifier in &order {
            if let Some(source) = self.get_source(specifier) {
                result.push(ModuleEntry {
                    specifier: specifier.clone(),
                    source: source.to_string(),
                });
            }
        }
        result
    }

    /// Returns whether a module's source has been decompressed.
    #[cfg(test)]
    fn is_decompressed(&self, specifier: &str) -> bool {
        self.modules.get(specifier).is_some_and(|m| m.decompressed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_modules() -> Vec<(String, ModuleType, String)> {
        vec![
            ("index.js".into(), ModuleType::EsModule, "export default 42;".into()),
            ("utils.js".into(), ModuleType::EsModule, "export function add(a,b){return a+b}".into()),
            ("data.json".into(), ModuleType::Json, r#"{"key":"value"}"#.into()),
        ]
    }

    #[test]
    fn round_trip() {
        let mods = sample_modules();
        let bundle = AppBundle::new("index.js", mods.clone());
        let bytes = bundle.to_bytes();
        let mut loaded = AppBundle::from_bytes(&bytes).unwrap();

        for (specifier, _, source) in &mods {
            let got = loaded.get_source(specifier).unwrap();
            assert_eq!(got, source, "source mismatch for {specifier}");
        }
    }

    #[test]
    fn lazy_decompression() {
        let mods = vec![
            ("a.js".into(), ModuleType::EsModule, "console.log('a');".into()),
            ("b.js".into(), ModuleType::EsModule, "console.log('b');".into()),
        ];
        let bundle = AppBundle::new("a.js", mods);
        let bytes = bundle.to_bytes();
        let mut loaded = AppBundle::from_bytes(&bytes).unwrap();

        // Neither decompressed yet.
        assert!(!loaded.is_decompressed("a.js"));
        assert!(!loaded.is_decompressed("b.js"));

        // Access only a.js.
        loaded.get_source("a.js").unwrap();
        assert!(loaded.is_decompressed("a.js"));
        assert!(!loaded.is_decompressed("b.js"));
    }

    #[test]
    fn integrity_check() {
        let mods = sample_modules();
        let bundle = AppBundle::new("index.js", mods);
        let mut bytes = bundle.to_bytes();

        // Corrupt one byte in the data section (last byte).
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;

        let result = AppBundle::from_bytes(&bytes);
        assert!(matches!(result, Err(BundleError::HashMismatch)));
    }

    #[test]
    fn entry_is_first() {
        let mods = sample_modules();
        let bundle = AppBundle::new("index.js", mods);
        let bytes = bundle.to_bytes();
        let loaded = AppBundle::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.entry(), "index.js");
    }

    #[test]
    fn unused_module_with_invalid_content() {
        // Module "bad.js" has content that is valid UTF-8 but we'll verify
        // it's never decompressed when we only access "good.js".
        let mods = vec![
            ("good.js".into(), ModuleType::EsModule, "export const x = 1;".into()),
            ("bad.js".into(), ModuleType::EsModule, "this is fine as source".into()),
        ];
        let bundle = AppBundle::new("good.js", mods);
        let mut bytes = bundle.to_bytes();

        // Now corrupt the compressed data for "bad.js" in the data section.
        // We know "good.js" is first in the data section. We need to find
        // where "bad.js" compressed data starts and corrupt it.
        // Re-parse to find the offset.
        let parsed = AppBundle::from_bytes(&bytes).unwrap();
        let bad_mod = &parsed.modules["bad.js"];
        // data section starts after the index. We need the absolute offset.
        // The data section in `bytes` starts at (bytes.len() - parsed.data.len()).
        let data_section_start = bytes.len() - parsed.data.len();
        let corrupt_pos = data_section_start + bad_mod.data_offset as usize;
        bytes[corrupt_pos] ^= 0xFF;

        // Re-hash so integrity passes (we want to test lazy decompression,
        // not integrity).
        let hash: [u8; 32] = Sha256::digest(&bytes[40..]).into();
        bytes[8..40].copy_from_slice(&hash);

        let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
        // Accessing "good.js" should work fine.
        assert_eq!(loaded.get_source("good.js").unwrap(), "export const x = 1;");
        // "bad.js" was never decompressed.
        assert!(!loaded.is_decompressed("bad.js"));
    }

    #[test]
    fn empty_bundle() {
        let bundle = AppBundle::new("index.js", vec![
            ("index.js".into(), ModuleType::EsModule, "x".into()),
        ]);
        let mut bytes = bundle.to_bytes();

        // Patch module count to 0 and rehash.
        // Module count is at byte 40..44.
        bytes[40..44].copy_from_slice(&0u32.to_le_bytes());
        let hash: [u8; 32] = Sha256::digest(&bytes[40..]).into();
        bytes[8..40].copy_from_slice(&hash);

        let result = AppBundle::from_bytes(&bytes);
        assert!(matches!(result, Err(BundleError::EmptyBundle)));
    }

}

#[cfg(test)]
mod stress_tests {
    use super::*;

    /// Generate a realistic JS module with functions, imports, and comments
    fn generate_realistic_module(name: &str, size_kb: usize) -> String {
        let mut source = format!("// Module: {name}\n");
        source.push_str("'use strict';\n\n");

        // Add some imports
        source.push_str("// Simulated imports (would be resolved by bundler)\n");

        // Generate functions until we hit target size
        let mut fn_count = 0;
        while source.len() < size_kb * 1024 {
            source.push_str(&format!(
                r#"
export function handler_{fn_count}(req) {{
    const data = JSON.parse(req.body);
    const result = {{
        id: data.id || Math.random().toString(36).slice(2),
        name: data.name || "default",
        timestamp: Date.now(),
        processed: true,
        metadata: {{
            handler: "handler_{fn_count}",
            module: "{name}",
            version: "1.0.0"
        }}
    }};
    return new Response(JSON.stringify(result), {{
        headers: {{ "Content-Type": "application/json" }}
    }});
}}
"#
            ));
            fn_count += 1;
        }

        source.push_str(&format!("\n// Module {name}: {fn_count} handlers, {} bytes\n", source.len()));
        source
    }

    #[test]
    fn stress_single_large_module() {
        // Single 500KB module (typical bundled app)
        let source = generate_realistic_module("app", 500);
        let original_len = source.len();

        let bundle = AppBundle::new("app.js", vec![
            ("app.js".into(), ModuleType::EsModule, source.clone()),
        ]);

        let bytes = bundle.to_bytes();
        let compression_ratio = bytes.len() as f64 / original_len as f64;

        eprintln!("Single 500KB module:");
        eprintln!("  Original: {} bytes", original_len);
        eprintln!("  Bundle:   {} bytes", bytes.len());
        eprintln!("  Ratio:    {:.1}%", compression_ratio * 100.0);

        // Verify round-trip
        let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
        let got = loaded.get_source("app.js").unwrap();
        assert_eq!(got, source);
        assert!(compression_ratio < 0.5, "Should compress to <50%");
    }

    #[test]
    fn stress_many_small_modules() {
        // 1000 small modules (unbundled app with many files)
        let mut modules = Vec::new();
        let mut total_source_size = 0;

        // Entry module imports 10 modules
        let mut entry = String::from("// Entry module\n");
        for i in 0..10 {
            entry.push_str(&format!("import {{ handler_{i} }} from './mod_{i}.js';\n"));
        }
        entry.push_str("export function ping() { return 'pong'; }\n");
        total_source_size += entry.len();
        modules.push(("index.js".into(), ModuleType::EsModule, entry));

        // 999 modules of varying sizes
        for i in 0..999 {
            let size_kb = (i % 10) + 1; // 1KB to 10KB
            let source = generate_realistic_module(&format!("mod_{i}"), size_kb);
            total_source_size += source.len();
            modules.push((format!("mod_{i}.js"), ModuleType::EsModule, source));
        }

        let bundle = AppBundle::new("index.js", modules);
        let bytes = bundle.to_bytes();

        eprintln!("\n1000 modules:");
        eprintln!("  Total source: {} bytes ({:.1} MB)", total_source_size, total_source_size as f64 / 1_048_576.0);
        eprintln!("  Bundle size:  {} bytes ({:.1} MB)", bytes.len(), bytes.len() as f64 / 1_048_576.0);
        eprintln!("  Ratio:        {:.1}%", bytes.len() as f64 / total_source_size as f64 * 100.0);

        // Parse and measure
        let start = std::time::Instant::now();
        let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
        let parse_time = start.elapsed();
        eprintln!("  Parse time:   {:?}", parse_time);

        assert_eq!(loaded.entry(), "index.js");

        // Access only the entry module (lazy — others untouched)
        let start = std::time::Instant::now();
        let entry_source = loaded.get_source("index.js").unwrap();
        let decompress_time = start.elapsed();
        eprintln!("  Entry decompress: {:?}", decompress_time);
        assert!(entry_source.contains("ping"));

        // Verify only entry was decompressed
        assert!(loaded.is_decompressed("index.js"));
        assert!(!loaded.is_decompressed("mod_0.js"));
        assert!(!loaded.is_decompressed("mod_999.js"));

        // Access 10 imported modules
        let start = std::time::Instant::now();
        for i in 0..10 {
            loaded.get_source(&format!("mod_{i}.js")).unwrap();
        }
        let import_time = start.elapsed();
        eprintln!("  10 imports decompress: {:?}", import_time);

        // Verify 990 modules still compressed
        assert!(!loaded.is_decompressed("mod_10.js"));
        assert!(!loaded.is_decompressed("mod_500.js"));
        assert!(!loaded.is_decompressed("mod_999.js"));

        // Performance assertions
        assert!(parse_time.as_millis() < 100, "Parse should be <100ms, got {:?}", parse_time);
        assert!(decompress_time.as_millis() < 10, "Decompress should be <10ms, got {:?}", decompress_time);
    }

    #[test]
    fn stress_mixed_module_types() {
        let mut modules = Vec::new();

        // ES module entry
        modules.push(("index.js".into(), ModuleType::EsModule,
            "import config from './config.json';\nimport tmpl from './template.txt';\nexport function ping() { return config.name; }".into()));

        // JSON config
        modules.push(("config.json".into(), ModuleType::Json,
            r#"{"name":"my-app","version":"1.0.0","settings":{"debug":false,"maxRetries":3}}"#.into()));

        // Text template
        modules.push(("template.txt".into(), ModuleType::Text,
            "Hello, {{name}}! Welcome to {{app}}.".into()));

        // Large data blob (simulated binary as text for this test)
        let data = "x".repeat(100_000);
        modules.push(("large-data.bin".into(), ModuleType::Data, data.clone()));

        let bundle = AppBundle::new("index.js", modules);
        let bytes = bundle.to_bytes();

        eprintln!("\nMixed types:");
        eprintln!("  Bundle size: {} bytes", bytes.len());

        let mut loaded = AppBundle::from_bytes(&bytes).unwrap();

        assert_eq!(loaded.get_source("index.js").unwrap(),
            "import config from './config.json';\nimport tmpl from './template.txt';\nexport function ping() { return config.name; }");
        assert_eq!(loaded.get_source("config.json").unwrap(),
            r#"{"name":"my-app","version":"1.0.0","settings":{"debug":false,"maxRetries":3}}"#);
        assert_eq!(loaded.get_source("template.txt").unwrap(),
            "Hello, {{name}}! Welcome to {{app}}.");
        assert_eq!(loaded.get_source("large-data.bin").unwrap(), data);
    }

    #[test]
    fn stress_repeated_integrity_check() {
        // Verify integrity check catches corruption at various positions
        let modules = vec![
            ("a.js".into(), ModuleType::EsModule, "export const a = 1;".repeat(100)),
            ("b.js".into(), ModuleType::EsModule, "export const b = 2;".repeat(100)),
            ("c.js".into(), ModuleType::EsModule, "export const c = 3;".repeat(100)),
        ];
        let bundle = AppBundle::new("a.js", modules);
        let original = bundle.to_bytes();

        // Corrupt every 100th byte after the header — hash check should catch all
        let total_positions = (original.len() - 40) / 100;
        for pos in (40..original.len()).step_by(100) {
            let mut bytes = original.clone();
            bytes[pos] ^= 0xFF;
            let result = AppBundle::from_bytes(&bytes);
            assert!(result.is_err(), "Corruption at byte {pos} was not detected (file len={})", original.len());
        }
        eprintln!("\nIntegrity stress: all {total_positions} corruptions detected");
    }
}
