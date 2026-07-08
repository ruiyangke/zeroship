use super::ProviderRegistry;

pub mod lite;
pub mod openmeter;
pub mod stripe_invoice;
pub mod stripe_meters;
mod stripe_webhook;

pub fn register_builtin(registry: &mut ProviderRegistry) {
    registry.register("lite", lite::factory);
    registry.register("openmeter", openmeter::factory);
    registry.register("stripe_meters", stripe_meters::factory);
    registry.register("stripe_invoice", stripe_invoice::factory);
}
