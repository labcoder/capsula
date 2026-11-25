//! The shell's handle on the core's strings. GPUI's global lives here, because neither the
//! trait nor the catalog's type is the shell's own.

use std::fmt;

use gpui_kit::Global;
pub use kumochi_shell_core::strings::Text;

/// The loaded catalog, kept as a GPUI global on the UI thread.
pub struct Strings(kumochi_shell_core::strings::Strings);

impl Global for Strings {}

impl fmt::Debug for Strings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Strings").finish_non_exhaustive()
    }
}

impl std::ops::Deref for Strings {
    type Target = kumochi_shell_core::strings::Strings;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Strings {
    /// Loads the English catalog compiled into the binary.
    pub fn english() -> Self {
        Self(kumochi_shell_core::strings::Strings::english())
    }
}
