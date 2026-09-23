//! Authenticated app subscriptions and value-free CDC invalidations.
//!
//! Each WebSocket message contains one protocol message. A connection serves
//! one app AND one database: an app may hold a binding to several, and two of
//! them may each declare a collection of the same name, so a request naming
//! only the app would leave the relay to guess which stream the subscriber
//! meant. Reconnection always requires a fresh snapshot; there is no durable
//! event replay contract. The relay sends no row values or primary keys.
//!
//! The connection carries the database, so [`Event`] does not: every event on
//! one socket belongs to the database its [`Subscribe`] named.

pub const PATH: &str = "/internal/v1/cdc/subscribe";
pub const MAX_MESSAGE_BYTES: usize = 8192;
const VERSION: u8 = 1;
const MAX_NAME_BYTES: usize = 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolError;

/// The assertion is never included in diagnostics.
pub struct Subscribe {
    pub app_id: String,
    /// The database this connection streams. The app is the authorization
    /// subject; the database is the target, and the relay refuses one the app
    /// holds no live binding to.
    pub database_id: String,
    pub authorization: String,
}

impl std::fmt::Debug for Subscribe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscribe")
            .field("app_id", &self.app_id)
            .field("database_id", &self.database_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Insert,
    Update,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Ready,
    Change {
        collection: String,
        operation: Operation,
    },
    Resync,
    Heartbeat,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
}

fn text(bytes: &[u8]) -> Result<&str, ProtocolError> {
    std::str::from_utf8(bytes).map_err(|_| ProtocolError)
}

impl Subscribe {
    /// Encode a bounded protocol message.
    ///
    /// Layout: `[VERSION, app_len, database_len, app, database, authorization]`.
    /// Both names are length-prefixed, so neither can run into the other or
    /// into the assertion that fills the remainder.
    ///
    /// # Errors
    /// Returns `ProtocolError` when a field exceeds its limit or is invalid.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        if !valid_name(&self.app_id)
            || !valid_name(&self.database_id)
            || self.authorization.is_empty()
            || self.authorization.len() + self.app_id.len() + self.database_id.len() + 3
                > MAX_MESSAGE_BYTES
        {
            return Err(ProtocolError);
        }
        let app_len = u8::try_from(self.app_id.len()).map_err(|_| ProtocolError)?;
        let database_len = u8::try_from(self.database_id.len()).map_err(|_| ProtocolError)?;
        let mut bytes = vec![VERSION, app_len, database_len];
        bytes.extend_from_slice(self.app_id.as_bytes());
        bytes.extend_from_slice(self.database_id.as_bytes());
        bytes.extend_from_slice(self.authorization.as_bytes());
        Ok(bytes)
    }

    /// Decode one complete protocol message.
    ///
    /// # Errors
    /// Returns `ProtocolError` for malformed, unknown, or oversized messages.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(ProtocolError);
        }
        let [VERSION, app_len, database_len, rest @ ..] = bytes else {
            return Err(ProtocolError);
        };
        let (app, rest) = rest
            .split_at_checked(usize::from(*app_len))
            .ok_or(ProtocolError)?;
        let (database, auth) = rest
            .split_at_checked(usize::from(*database_len))
            .ok_or(ProtocolError)?;
        let app_id = text(app)?;
        let database_id = text(database)?;
        let authorization = text(auth)?;
        if !valid_name(app_id) || !valid_name(database_id) || authorization.is_empty() {
            return Err(ProtocolError);
        }
        Ok(Self {
            app_id: app_id.into(),
            database_id: database_id.into(),
            authorization: authorization.into(),
        })
    }
}

impl Event {
    /// Encode a bounded protocol message.
    ///
    /// # Errors
    /// Returns `ProtocolError` when a field exceeds its limit or is invalid.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        Ok(match self {
            Self::Ready => vec![1],
            Self::Resync => vec![3],
            Self::Heartbeat => vec![4],
            Self::Change {
                collection,
                operation,
            } => {
                if !valid_name(collection) {
                    return Err(ProtocolError);
                }
                let operation = match operation {
                    Operation::Insert => 1,
                    Operation::Update => 2,
                    Operation::Delete => 3,
                };
                let mut bytes = vec![2, operation];
                bytes.extend_from_slice(collection.as_bytes());
                bytes
            }
        })
    }

    /// Decode one complete protocol message.
    ///
    /// # Errors
    /// Returns `ProtocolError` for malformed, unknown, or oversized messages.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        match bytes {
            [1] => Ok(Self::Ready),
            [3] => Ok(Self::Resync),
            [4] => Ok(Self::Heartbeat),
            [2, op, name @ ..] => {
                let collection = text(name)?;
                if !valid_name(collection) {
                    return Err(ProtocolError);
                }
                let operation = match op {
                    1 => Operation::Insert,
                    2 => Operation::Update,
                    3 => Operation::Delete,
                    _ => return Err(ProtocolError),
                };
                Ok(Self::Change {
                    collection: collection.into(),
                    operation,
                })
            }
            _ => Err(ProtocolError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_bounded_and_redacts_the_assertion() {
        let request = Subscribe {
            app_id: "app_fixture".into(),
            database_id: "dbs_fixture".into(),
            authorization: "Bearer secret".into(),
        };
        let encoded = request.encode().unwrap();
        let decoded = Subscribe::decode(&encoded).unwrap();
        assert_eq!(decoded.app_id, request.app_id);
        assert_eq!(decoded.database_id, request.database_id);
        assert_eq!(decoded.authorization, request.authorization);
        assert!(!format!("{request:?}").contains("secret"));
        for bytes in [
            &[][..],
            // Wrong version byte.
            &[0, 1, 1, b'a', b'b', b'x'],
            // An app length that overruns the buffer.
            &[1, 2, 1, b'a'],
            // A database length that overruns what the app left.
            &[1, 1, 4, b'a', b'b'],
            // Empty app name, empty database name, empty assertion.
            &[1, 0, 1, b'b', b'x'],
            &[1, 1, 0, b'a', b'x'],
            &[1, 1, 1, b'a', b'b'],
        ] {
            assert!(Subscribe::decode(bytes).is_err());
        }
        assert!(Subscribe::decode(&vec![1; MAX_MESSAGE_BYTES + 1]).is_err());
    }

    /// **The two names cannot be transposed into each other.**
    ///
    /// Both are length-prefixed and the prefixes are read in a fixed order, so
    /// a request whose halves are swapped decodes to the swap rather than to
    /// the original - which is what makes the database half a real field
    /// rather than a suffix the app half could absorb.
    #[test]
    fn the_app_and_the_database_are_separately_framed() {
        let request = Subscribe {
            app_id: "app_one".into(),
            database_id: "dbs_two".into(),
            authorization: "Bearer secret".into(),
        };
        let swapped = Subscribe {
            app_id: request.database_id.clone(),
            database_id: request.app_id.clone(),
            authorization: request.authorization.clone(),
        };
        assert_ne!(request.encode().unwrap(), swapped.encode().unwrap());
        let decoded = Subscribe::decode(&swapped.encode().unwrap()).unwrap();
        assert_eq!(decoded.app_id, "dbs_two");
        assert_eq!(decoded.database_id, "app_one");

        // Names of different lengths must not let one borrow the other's
        // bytes: the assertion is whatever remains after BOTH prefixes.
        let uneven = Subscribe {
            app_id: "a".into(),
            database_id: "dbs_long_name".into(),
            authorization: "Bearer secret".into(),
        };
        let decoded = Subscribe::decode(&uneven.encode().unwrap()).unwrap();
        assert_eq!(decoded.app_id, "a");
        assert_eq!(decoded.database_id, "dbs_long_name");
        assert_eq!(decoded.authorization, "Bearer secret");
    }

    #[test]
    fn all_events_roundtrip_and_reject_trailing_payloads() {
        let mut events = vec![Event::Ready, Event::Resync, Event::Heartbeat];
        for operation in [Operation::Insert, Operation::Update, Operation::Delete] {
            events.push(Event::Change {
                collection: "orders".into(),
                operation,
            });
            events.push(Event::Change {
                collection: "__zeroship_private".into(),
                operation,
            });
        }
        for event in events {
            assert_eq!(Event::decode(&event.encode().unwrap()), Ok(event));
        }
        for bytes in [
            &[][..],
            &[1, 0],
            &[3, 0],
            &[4, 0],
            &[2, 0, b'a'],
            &[2, 1],
            &[2, 1, 255],
            &[255],
        ] {
            assert!(Event::decode(bytes).is_err());
        }
        for name in ["bad\0name", &"x".repeat(MAX_NAME_BYTES + 1)] {
            let event = Event::Change {
                collection: name.into(),
                operation: Operation::Insert,
            };
            assert!(event.encode().is_err());
        }
    }
}
