//! Content hashes of linked code: SHA-256 digests (03 R6, 02).

use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

/// A SHA-256 digest. Parsed from 64 hex characters (either case); always rendered lowercase.
///
/// Storing the bytes instead of hex text means an invalid hash can't be represented, and
/// comparisons can't be thrown off by upper- versus lower-case hex.
#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct Hash([u8; 32]);

impl Hash {
    /// The number of hex characters in a rendered hash.
    pub const HEX_LEN: usize = 64;

    /// The SHA-256 digest of `bytes`.
    #[must_use]
    pub fn digest(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Wraps an already computed digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl FromStr for Hash {
    type Err = HashParseError;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        let invalid = || HashParseError {
            input: hex.to_owned(),
        };
        if hex.len() != Self::HEX_LEN {
            return Err(invalid());
        }
        let mut bytes = [0_u8; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
            *byte = (nibble(*high).ok_or_else(invalid)? << 4) | nibble(*low).ok_or_else(invalid)?;
        }
        Ok(Self(bytes))
    }
}

/// The value of one hex digit, in either case.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

impl TryFrom<String> for Hash {
    type Error = HashParseError;

    fn try_from(hex: String) -> Result<Self, Self::Error> {
        hex.parse()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

impl schemars::JsonSchema for Hash {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Hash".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A SHA-256 digest as 64 lowercase hex characters.",
            "pattern": "^[0-9a-f]{64}$",
        })
    }
}

impl serde::Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A string is not 64 hex characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid hash `{input}`: expected 64 hex characters")]
pub struct HashParseError {
    /// The rejected text, as written.
    input: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of the empty input, from FIPS 180-2 test vectors.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn digest_is_sha256_rendered_as_lowercase_hex() {
        assert_eq!(Hash::digest(b"").to_string(), EMPTY_SHA256);
    }

    #[test]
    fn uppercase_hash_is_accepted_and_rendered_lowercase() {
        let parsed: Hash = EMPTY_SHA256
            .to_ascii_uppercase()
            .parse()
            .expect("upper-case hex should be accepted");

        assert_eq!(parsed, Hash::digest(b""));
        assert_eq!(parsed.to_string(), EMPTY_SHA256);
    }

    #[test]
    fn short_long_or_non_hex_hash_is_rejected() {
        let short = "0".repeat(63);
        let long = "0".repeat(65);
        let not_hex = EMPTY_SHA256.replacen('e', "g", 1);
        // 64 bytes, but `é` is two of them, so the byte length alone can't accept it.
        let multibyte = format!("é{}", "0".repeat(62));
        for input in ["9f2c", &short, &long, &not_hex, &multibyte] {
            assert_eq!(
                input.parse::<Hash>().map_err(|error| error.to_string()),
                Err(format!(
                    "invalid hash `{input}`: expected 64 hex characters"
                )),
                "`{input}` should be rejected",
            );
        }
    }

    #[test]
    fn bytes_round_trip() {
        let hash = Hash::digest(b"fn add() {}");

        assert_eq!(Hash::from_bytes(*hash.as_bytes()), hash);
    }

    #[test]
    fn hash_deserializes_and_serializes_as_hex_text() {
        let quoted = format!("\"{EMPTY_SHA256}\"");
        let parsed: Hash = serde_json::from_str(&quoted).expect("a hex string should deserialize");

        assert_eq!(
            serde_json::to_string(&parsed).expect("a hash should serialize"),
            quoted,
        );
    }
}
