//! Re-export SpendAction from core for backward compatibility.
pub use crate::core::billing::SpendAction;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU8;

    #[test]
    fn roundtrip() {
        for action in [SpendAction::Allow, SpendAction::Warn, SpendAction::Degrade, SpendAction::Block] {
            assert_eq!(SpendAction::from_u8(action as u8), action);
        }
    }

    #[test]
    fn unknown_maps_to_block() {
        assert_eq!(SpendAction::from_u8(255), SpendAction::Block);
    }

    #[test]
    fn atomic_load_store() {
        let atom = AtomicU8::new(0);
        assert_eq!(SpendAction::load(&atom), SpendAction::Allow);
        SpendAction::store(&atom, SpendAction::Warn);
        assert_eq!(SpendAction::load(&atom), SpendAction::Warn);
    }
}
