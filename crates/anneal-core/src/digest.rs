//! Content digests — the identity of every artifact (§1.5 content-addressing).

use std::fmt;

use sha2::{Digest as _, Sha256};

/// A content address: the SHA-256 of some bytes.
///
/// Content-addressing is the single mechanism behind caching, deduplication, and
/// reproducibility (§1.5), so this is the most pervasive vocabulary type. The inner
/// bytes are private: a `Digest` can only be produced by hashing content
/// ([`Digest::of`]) or by parsing a known-good hex string ([`Digest::from_hex`]),
/// never by stuffing in arbitrary bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

impl Digest {
    /// The content address of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&out);
        Digest(buf)
    }

    /// Parse a 64-character lowercase/uppercase hex string.
    pub fn from_hex(s: &str) -> Result<Self, DigestParseError> {
        if s.len() != 64 {
            return Err(DigestParseError::BadLength(s.len()));
        }
        let bytes = s.as_bytes();
        let mut buf = [0u8; 32];
        for (i, slot) in buf.iter_mut().enumerate() {
            let hi = hex_nibble(bytes[2 * i])?;
            let lo = hex_nibble(bytes[2 * i + 1])?;
            *slot = (hi << 4) | lo;
        }
        Ok(Digest(buf))
    }

    /// The 64-character lowercase hex representation.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            // Two lowercase hex digits per byte.
            const HEX: &[u8; 16] = b"0123456789abcdef";
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0xf) as usize] as char);
        }
        s
    }

    /// The raw 32 hash bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn hex_nibble(c: u8) -> Result<u8, DigestParseError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        other => Err(DigestParseError::BadChar(other as char)),
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short form keeps logs readable; full form is available via Display.
        write!(f, "Digest({}…)", &self.to_hex()[..12])
    }
}

/// Failure parsing a [`Digest`] from hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestParseError {
    /// Expected 64 hex characters; found this many.
    BadLength(usize),
    /// Encountered a non-hex character.
    BadChar(char),
}

impl fmt::Display for DigestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DigestParseError::BadLength(n) => {
                write!(f, "expected 64 hex characters, found {n}")
            }
            DigestParseError::BadChar(c) => write!(f, "invalid hex character {c:?}"),
        }
    }
}

impl std::error::Error for DigestParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn of_is_stable_and_distinguishing() {
        assert_eq!(Digest::of(b"hello"), Digest::of(b"hello"));
        assert_ne!(Digest::of(b"hello"), Digest::of(b"world"));
    }

    #[test]
    fn known_vector() {
        // SHA-256 of the empty string.
        assert_eq!(
            Digest::of(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hex_round_trip() {
        let d = Digest::of(b"some content");
        assert_eq!(Digest::from_hex(&d.to_hex()).unwrap(), d);
    }

    #[test]
    fn from_hex_rejects_bad_input() {
        assert_eq!(Digest::from_hex("abc"), Err(DigestParseError::BadLength(3)));
        let mut bad = "a".repeat(64);
        bad.replace_range(0..1, "z");
        assert_eq!(Digest::from_hex(&bad), Err(DigestParseError::BadChar('z')));
    }
}
