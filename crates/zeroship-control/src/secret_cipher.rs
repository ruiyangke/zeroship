//! Control-owned wrapping keys shared by secret stores.

use zeroize::{Zeroize, Zeroizing};
use zeroship_core::crypto::{self, CryptoError};

pub(crate) struct SecretCipher {
    pub(crate) primary: [u8; 32],
    pub(crate) previous: Vec<[u8; 32]>,
}

impl SecretCipher {
    pub(crate) fn new(primary: &str, previous: &[&str]) -> Self {
        Self {
            primary: crypto::derive_key(primary),
            previous: previous
                .iter()
                .filter(|key| !key.is_empty())
                .map(|key| crypto::derive_key(key))
                .collect(),
        }
    }

    pub(crate) fn seal(&self, aad: &[u8], value: &[u8]) -> Result<Vec<u8>, CryptoError> {
        crypto::encrypt(&self.primary, aad, value)
    }

    pub(crate) fn open(
        &self,
        aad: &[u8],
        ciphertext: &[u8],
    ) -> Result<(Zeroizing<Vec<u8>>, bool), CryptoError> {
        if let Ok(value) = crypto::decrypt(&self.primary, aad, ciphertext) {
            return Ok((Zeroizing::new(value), false));
        }
        crypto::decrypt_with_keys(&self.previous, aad, ciphertext)
            .map(|value| (Zeroizing::new(value), true))
    }
}

impl Drop for SecretCipher {
    fn drop(&mut self) {
        self.primary.zeroize();
        self.previous.zeroize();
    }
}
