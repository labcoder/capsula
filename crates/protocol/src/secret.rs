//! A wrapper that keeps secrets out of logs and debug output.

use std::fmt;

/// A value that prints as `***`. Read it only at the moment of use, through [`Secret::expose`].
///
/// It deliberately has no `Display`, so it can't be formatted into user text or a log line.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wraps a secret value.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// The secret itself. Keep the borrow short and never log it.
    pub const fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn debug_output_never_contains_the_value() {
        let secret = Secret::new(String::from("sk-test-123"));
        assert_eq!(format!("{secret:?}"), "***");
        assert_eq!(format!("{:?}", Some(&secret)), "Some(***)");
        assert_eq!(secret.expose(), "sk-test-123");
    }
}
