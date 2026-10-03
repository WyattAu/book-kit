//! FNV-1a-64 — the integer-only checksum primitive (REQ-BOOK-010).
//!
//! The spec fixes FNV-1a-64 as the dependency-free checksum (§Decisions 5).
//! It is a *detection* hash, not adversarial protection: collisions are
//! possible and acceptable for snapshot verification and replay-equivalence
//! checks. No `f64` anywhere (REQ-BOOK-004).

/// FNV-1a 64-bit offset basis.
const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Incremental FNV-1a-64 hasher over little-endian `u64` words.
///
/// Every word of book state is hashed in a fixed field order, so the
/// checksum is a pure function of the state (REQ-BOOK-010 determinism).
pub(crate) struct Fnv1a {
    state: u64,
}

impl Fnv1a {
    pub(crate) const fn new() -> Self {
        Self { state: OFFSET }
    }

    /// Absorb one `u64` as 8 little-endian bytes.
    pub(crate) fn write_u64(&mut self, v: u64) {
        let mut i = 0;
        while i < 8 {
            self.state ^= (v >> (i * 8)) as u8 as u64;
            self.state = self.state.wrapping_mul(PRIME);
            i += 1;
        }
    }

    pub(crate) fn finish(self) -> u64 {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a-64 of the empty input is the offset basis (published vector).
    #[test]
    fn fnv_empty_input_is_offset_basis() {
        assert_eq!(Fnv1a::new().finish(), 0xcbf2_9ce4_8422_2325);
    }

    /// Published-shape FNV-1a-64: hash the 8 LE bytes of `0x61` and match
    /// a hand-rolled loop (guards accidental primitive/word drift).
    #[test]
    fn fnv_known_vector_a() {
        let bytes = [b'a', 0, 0, 0, 0, 0, 0, 0];
        let mut v = OFFSET;
        for &b in &bytes {
            v ^= b as u64;
            v = v.wrapping_mul(PRIME);
        }
        let mut h = Fnv1a::new();
        h.write_u64(u64::from_le_bytes(bytes));
        assert_eq!(h.finish(), v);
    }

    /// Word order matters: absorbing u64s in different orders diverges.
    #[test]
    fn fnv_word_order_sensitive() {
        let mut a = Fnv1a::new();
        a.write_u64(1);
        a.write_u64(2);
        let mut b = Fnv1a::new();
        b.write_u64(2);
        b.write_u64(1);
        assert_ne!(a.finish(), b.finish());
    }
}
