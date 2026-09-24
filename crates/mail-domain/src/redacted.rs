//! A wrapper for secrets that must never reach logs (spec §12, §15).

use std::fmt;

/// Holds a secret whose `Debug` and `Display` print `***`, so it can sit in
/// structs that derive `Debug` or be passed to `tracing` fields safely.
/// Read the value explicitly with [`Redacted::expose`].
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// The secret itself. Call sites are easy to audit with `grep expose`.
    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Redacted<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    #[allow(dead_code)]
    struct Token {
        user: &'static str,
        secret: Redacted<String>,
    }

    #[test]
    fn debug_and_display_hide_the_value() {
        let t = Token { user: "a@example.com", secret: Redacted::new("ya29.secret".into()) };
        let printed = format!("{t:?} {}", t.secret);
        assert!(!printed.contains("ya29"), "{printed}");
        assert!(printed.contains("***"));
        assert_eq!(t.secret.expose(), "ya29.secret");
    }
}
