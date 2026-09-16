//! Crypto helpers shared by the WebCrypto and node:crypto surfaces.
//!
//! Native `Crypto` / `SubtleCrypto` / `CryptoKey` (`web::crypto`) own
//! the WebCrypto JS surface. Native `Hash` / `Hmac` / random / KDFs
//! (`node::crypto`) own the node:crypto JS surface. This file holds
//! only:
//!
//! 1. `fast_random` — thread-local 4 KB CSPRNG buffer (workerd's
//!    OPENSSL_cleanse pattern), called from `web::crypto::helpers`,
//!    `node::crypto::random`, and other call sites that need amortised
//!    CSPRNG bytes.

use std::cell::RefCell;

// ---------------------------------------------------------------------------
// Thread-local entropy buffer (~4 KB, zeroed-after-dispense per workerd's
// OPENSSL_cleanse pattern). Amortises CSPRNG syscall across many small
// random reads (UUID gen, masking keys, salt, etc.).
// ---------------------------------------------------------------------------

const ENTROPY_BUF_SIZE: usize = 4096;

#[inline(never)]
fn zeroize_slice(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        // Volatile write so the compiler can't elide the store. Same
        // purpose as workerd's OPENSSL_cleanse: ensure consumed entropy
        // doesn't linger in memory where a side-channel could read it.
        #[allow(unsafe_code)]
        unsafe {
            std::ptr::write_volatile(byte as *mut u8, 0)
        };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

struct EntropyBuf {
    store: [u8; ENTROPY_BUF_SIZE],
    pos: usize,
}

impl EntropyBuf {
    fn new() -> Self {
        Self {
            store: [0u8; ENTROPY_BUF_SIZE],
            pos: ENTROPY_BUF_SIZE,
        }
    }

    fn fill(&mut self, out: &mut [u8]) {
        let mut remaining = out.len();
        let mut offset = 0;
        while remaining > 0 {
            if self.pos >= ENTROPY_BUF_SIZE {
                aws_lc_rs::rand::fill(&mut self.store).unwrap();
                self.pos = 0;
            }
            let avail = ENTROPY_BUF_SIZE - self.pos;
            let n = remaining.min(avail);
            out[offset..offset + n].copy_from_slice(&self.store[self.pos..self.pos + n]);
            zeroize_slice(&mut self.store[self.pos..self.pos + n]);
            self.pos += n;
            offset += n;
            remaining -= n;
        }
    }
}

thread_local! {
    static ENTROPY: RefCell<EntropyBuf> = RefCell::new(EntropyBuf::new());
}

pub(crate) fn fast_random(out: &mut [u8]) {
    ENTROPY.with(|e| e.borrow_mut().fill(out));
}
