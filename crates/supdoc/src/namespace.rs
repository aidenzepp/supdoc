//! Namespace names: the `auth` in a link like `[login](auth:src/lib.rs#login)`.

use std::fmt;
use std::str::FromStr;

/// A namespace name: an ASCII letter, then ASCII letters, digits, `-` or `_`, at least 2
/// characters long (01 D6).
///
/// The two-character minimum keeps a name from reading like a Windows drive (`C:src`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct NamespaceName(String);

impl NamespaceName {
    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NamespaceName {
    type Error = NamespaceNameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let mut chars = name.chars();
        if !chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
        {
            return Err(NamespaceNameError::InvalidStart { name });
        }
        if let Some(found) = chars.find(|&c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
            return Err(NamespaceNameError::InvalidCharacter { name, found });
        }
        if name.len() < 2 {
            return Err(NamespaceNameError::TooShort { name });
        }
        Ok(Self(name))
    }
}

impl FromStr for NamespaceName {
    type Err = NamespaceNameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::try_from(name.to_owned())
    }
}

impl fmt::Display for NamespaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for NamespaceName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Why a string is not a valid [`NamespaceName`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NamespaceNameError {
    /// The name is empty or starts with something other than an ASCII letter.
    #[error("namespace names must start with an ASCII letter (got '{name}')")]
    InvalidStart {
        /// The rejected name.
        name: String,
    },
    /// The name contains a character outside letters, digits, `-` and `_`.
    #[error(
        "namespace names may contain only ASCII letters, digits, `-` and `_` (got '{found}' in '{name}')"
    )]
    InvalidCharacter {
        /// The rejected name.
        name: String,
        /// The first character that is not allowed.
        found: char,
    },
    /// The name is a single letter.
    #[error("namespace names need at least 2 characters (got '{name}')")]
    TooShort {
        /// The rejected name.
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(name: &str) -> Result<NamespaceName, NamespaceNameError> {
        name.parse()
    }

    #[test]
    fn namespace_name_accepts_letters_digits_dash_underscore() {
        for name in ["auth", "ab", "Auth-2_x", "z9"] {
            let parsed =
                parse(name).unwrap_or_else(|error| panic!("`{name}` should be accepted: {error}"));
            assert_eq!(parsed.as_str(), name);
        }
    }

    #[test]
    fn namespace_name_rejects_single_letter() {
        assert_eq!(
            parse("c").map_err(|error| error.to_string()),
            Err("namespace names need at least 2 characters (got 'c')".to_owned()),
        );
    }

    #[test]
    fn namespace_name_rejects_leading_digit_and_symbols() {
        for name in ["", "1auth", "-auth", "_auth"] {
            assert!(
                matches!(parse(name), Err(NamespaceNameError::InvalidStart { .. })),
                "`{name}` should be rejected for its first character",
            );
        }
        for (name, found) in [
            ("au th", ' '),
            ("auth.v2", '.'),
            ("aüth", 'ü'),
            ("a:b", ':'),
        ] {
            assert_eq!(
                parse(name),
                Err(NamespaceNameError::InvalidCharacter {
                    name: name.to_owned(),
                    found
                }),
                "`{name}` should be rejected naming `{found}`",
            );
        }
    }

    #[test]
    fn namespace_name_deserializes_through_its_check() {
        let names: std::collections::BTreeMap<NamespaceName, String> =
            toml::from_str(r#"auth = "services/auth""#).expect("a valid name should deserialize");
        let rejected =
            toml::from_str::<std::collections::BTreeMap<NamespaceName, String>>(r#"c = "x""#);

        assert_eq!(
            names.keys().map(NamespaceName::as_str).collect::<Vec<_>>(),
            ["auth"]
        );
        assert!(
            rejected.is_err(),
            "a one-letter name should fail to deserialize: {rejected:?}"
        );
    }
}
