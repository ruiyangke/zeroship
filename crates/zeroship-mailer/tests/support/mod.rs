pub mod database;
pub mod smtp;

use zeroship_mailer::{Address, Email};

/// A recipient unique to one case, on the domain the Mailpit sink accepts.
///
/// Every case mints its own address so a suppression it writes, or a message it
/// delivers, is scoped to the rows it owns.
pub fn recipient(label: &str) -> String {
    format!("{label}-{}@personal.test", uuid::Uuid::new_v4().simple())
}

pub fn message(to: &str) -> Email {
    Email {
        to: Address {
            email: to.into(),
            name: Some("Reader".into()),
        },
        header_to: None,
        from: Address {
            email: "auth@zeroship.test".into(),
            name: Some("zeroship".into()),
        },
        reply_to: None,
        envelope_from: None,
        subject: "Confirm your address".into(),
        text: "Please confirm your address.\r\n.Start here.\r\nThank you.".into(),
        html: Some("<p>Please <strong>confirm</strong> your address.</p>".into()),
        headers: vec![],
        tags: vec![],
        idempotency_key: None,
    }
}
