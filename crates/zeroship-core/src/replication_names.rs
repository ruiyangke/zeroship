//! Stable names shared by migration-time and worker-time CDC code.

use sha2::{Digest, Sha256};

/// Prefix reserved for zeroship logical-replication objects.
pub const OBJECT_PREFIX: &str = "__zs_";

/// Why a caller-controlled application id cannot name a CDC object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplicationNameError {
    #[error("app id must not be empty")]
    EmptyAppId,
    #[error("app id must not contain NUL")]
    NulAppId,
}

/// Compose the stable per-app publication name.
pub fn publication_name(app_id: &str) -> Result<String, ReplicationNameError> {
    if app_id.is_empty() {
        return Err(ReplicationNameError::EmptyAppId);
    }
    if app_id.contains('\0') {
        return Err(ReplicationNameError::NulAppId);
    }

    let digest = Sha256::digest(app_id.as_bytes());
    let mut token = String::with_capacity(28);
    for byte in &digest[..14] {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("{OBJECT_PREFIX}pub_{token}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_names_are_stable_case_sensitive_identifiers() {
        assert_eq!(
            publication_name("alpha").unwrap(),
            "__zs_pub_8ed3f6ad685b959ead7022518e1a"
        );
        assert_ne!(
            publication_name("MyApp").unwrap(),
            publication_name("myapp").unwrap()
        );
        assert!(publication_name("").is_err());
        assert!(publication_name("bad\0app").is_err());
    }
}
