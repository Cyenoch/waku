//! Secret material that must never appear in Debug, Display, or logs.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use ts_rs::TS;

/// Wire-safe secret. Serde carries the value once; Debug/Display are redacted.
#[derive(Clone, TS)]
#[ts(type = "string")]
pub struct SecretString(String);

impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([redacted])")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_never_include_secret_bytes() {
        let secret = SecretString::new("sk-super-secret");
        assert!(!format!("{secret:?}").contains("sk-"));
        assert!(!format!("{secret}").contains("sk-"));
        assert_eq!(secret.expose(), "sk-super-secret");
    }

    #[test]
    fn serde_round_trips_the_value_for_the_login_command() {
        let secret = SecretString::new("sk-wire");
        let json = serde_json::to_string(&secret).unwrap();
        assert_eq!(json, "\"sk-wire\"");
        let parsed: SecretString = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.expose(), "sk-wire");
    }
}
