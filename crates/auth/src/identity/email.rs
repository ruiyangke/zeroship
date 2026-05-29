//! Shared email-address validation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailValidationError {
    Invalid,
}

pub fn validate_email(email: &str) -> Result<(), EmailValidationError> {
    if email.is_empty()
        || email.len() > 254
        || email.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(EmailValidationError::Invalid);
    }

    let mut parts = email.split('@');
    let Some(local) = parts.next() else {
        return Err(EmailValidationError::Invalid);
    };
    let Some(domain) = parts.next() else {
        return Err(EmailValidationError::Invalid);
    };
    if parts.next().is_some() || local.is_empty() || domain.is_empty() {
        return Err(EmailValidationError::Invalid);
    }

    if local.len() > 64 || !domain.contains('.') {
        return Err(EmailValidationError::Invalid);
    }
    if domain.split('.').any(str::is_empty) {
        return Err(EmailValidationError::Invalid);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_basic_mailbox() {
        assert_eq!(validate_email("ada@example.com"), Ok(()));
    }

    #[test]
    fn rejects_oversized_mailbox() {
        let email = format!("a@{}.com", "b".repeat(30_000));
        assert_eq!(validate_email(&email), Err(EmailValidationError::Invalid));
    }

    #[test]
    fn rejects_missing_or_empty_parts() {
        assert_eq!(validate_email("noatsign.com"), Err(EmailValidationError::Invalid));
        assert_eq!(validate_email("@nolocal.com"), Err(EmailValidationError::Invalid));
        assert_eq!(validate_email("local@"), Err(EmailValidationError::Invalid));
        assert_eq!(validate_email("local@@example.com"), Err(EmailValidationError::Invalid));
    }

    #[test]
    fn rejects_domain_without_dot() {
        assert_eq!(validate_email("local@example"), Err(EmailValidationError::Invalid));
    }
}
