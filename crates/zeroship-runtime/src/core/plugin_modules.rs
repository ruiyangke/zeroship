//! Plugin-owned adapter sources, kept separate from creator artifacts.

use std::collections::HashSet;
use std::sync::Arc;

use super::modules::ModuleEntry;
use super::plugin::{JavaScriptModule, NativePlugin};

#[derive(Clone, Default)]
pub struct PluginModules(pub Vec<JavaScriptModule>);

pub fn register(
    scope: &mut v8::PinScope<'_, '_>,
    plugins: &[Arc<dyn NativePlugin>],
    creator_modules: &[ModuleEntry],
) -> Result<(), String> {
    for entry in creator_modules {
        let specifier = entry.specifier.trim_start_matches("./");
        if specifier.starts_with("zeroship:")
            || specifier == "zeroship"
            || specifier == "zeroship.js"
        {
            return Err(format!(
                "runtime: creator module {:?} uses a reserved host specifier",
                entry.specifier
            ));
        }
    }
    let mut modules = Vec::new();
    let mut names = HashSet::new();
    for plugin in plugins {
        let prefix = format!("zeroship:{}/", plugin.namespace());
        for module in plugin.javascript_modules() {
            let suffix = module.specifier.strip_prefix(&prefix).ok_or_else(|| {
                format!(
                    "runtime: plugin {:?} must provide modules under {prefix:?}",
                    plugin.name()
                )
            })?;
            if suffix.is_empty()
                || suffix
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err(format!(
                    "runtime: invalid plugin module specifier {:?}",
                    module.specifier
                ));
            }
            if module.source.trim().is_empty() {
                return Err(format!(
                    "runtime: empty plugin module {:?}",
                    module.specifier
                ));
            }
            if !names.insert(module.specifier) {
                return Err(format!(
                    "runtime: duplicate plugin module {:?}",
                    module.specifier
                ));
            }
            modules.push(*module);
        }
    }
    scope.set_slot(PluginModules(modules));
    Ok(())
}
