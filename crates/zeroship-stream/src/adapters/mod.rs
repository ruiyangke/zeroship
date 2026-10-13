pub mod memory;
pub mod redpanda;

use crate::StreamRegistry;

pub fn register_builtin(registry: &mut StreamRegistry) {
    registry.register("memory", memory::factory);
    registry.register(redpanda::ID, redpanda::factory);
}
