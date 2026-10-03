pub mod database;
pub mod smtp;

use zeroship_mailer::{Address, Email};

pub fn message() -> Email {
    Email {
        to: Address {
            email: "reader@personal.test".into(),
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
