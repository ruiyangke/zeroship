//! Authenticated app subscriptions and value-free CDC invalidations.
//!
//! Each WebSocket message contains one protocol message. A connection serves
//! one app. Reconnection always requires a fresh snapshot; there is no durable
//! event replay contract. The relay sends no row values or primary keys.

pub const PATH: &str = "/internal/v1/cdc/subscribe";
pub const MAX_MESSAGE_BYTES: usize = 8192;
const VERSION: u8 = 1;
const MAX_NAME_BYTES: usize = 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolError;

/// The assertion is never included in diagnostics.
pub struct Subscribe {
    pub app_id: String,
    pub authorization: String,
}

impl std::fmt::Debug for Subscribe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscribe")
            .field("app_id", &self.app_id)
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
    /// # Errors
    /// Returns `ProtocolError` when a field exceeds its limit or is invalid.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        if !valid_name(&self.app_id)
            || self.authorization.is_empty()
            || self.authorization.len() + self.app_id.len() + 2 > MAX_MESSAGE_BYTES
        {
            return Err(ProtocolError);
        }
        let name_len = u8::try_from(self.app_id.len()).map_err(|_| ProtocolError)?;
        let mut bytes = vec![VERSION, name_len];
        bytes.extend_from_slice(self.app_id.as_bytes());
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
        let [VERSION, len, rest @ ..] = bytes else {
            return Err(ProtocolError);
        };
        let (app, auth) = rest
            .split_at_checked(usize::from(*len))
            .ok_or(ProtocolError)?;
        let app_id = text(app)?;
        let authorization = text(auth)?;
        if !valid_name(app_id) || authorization.is_empty() {
            return Err(ProtocolError);
        }
        Ok(Self {
            app_id: app_id.into(),
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
                if !valid_name(collection) || collection.starts_with("__") {
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
                if !valid_name(collection) || collection.starts_with("__") {
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
            authorization: "Bearer secret".into(),
        };
        let encoded = request.encode().unwrap();
        let decoded = Subscribe::decode(&encoded).unwrap();
        assert_eq!(decoded.app_id, request.app_id);
        assert_eq!(decoded.authorization, request.authorization);
        assert!(!format!("{request:?}").contains("secret"));
        for bytes in [&[][..], &[0, 1, b'a', b'x'], &[1, 2, b'a'], &[1, 0, b'x']] {
            assert!(Subscribe::decode(bytes).is_err());
        }
        assert!(Subscribe::decode(&vec![1; MAX_MESSAGE_BYTES + 1]).is_err());
    }

    #[test]
    fn all_events_roundtrip_and_reject_trailing_payloads() {
        let mut events = vec![Event::Ready, Event::Resync, Event::Heartbeat];
        for operation in [Operation::Insert, Operation::Update, Operation::Delete] {
            events.push(Event::Change {
                collection: "orders".into(),
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
        for name in [
            "__zeroship_private",
            "bad\0name",
            &"x".repeat(MAX_NAME_BYTES + 1),
        ] {
            let event = Event::Change {
                collection: name.into(),
                operation: Operation::Insert,
            };
            assert!(event.encode().is_err());
        }
    }
}
