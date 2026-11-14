//! The shell's handle on the core's strings. GPUI's global lives here, because neither the
//! trait nor the catalog's type is the shell's own.

use gpui_kit::Global;
pub(crate) use kumochi_shell_core::strings::Text;

/// The loaded catalog, kept as a GPUI global on the UI thread.
pub(crate) struct Strings(kumochi_shell_core::strings::Strings);

impl Global for Strings {}

impl std::ops::Deref for Strings {
    type Target = kumochi_shell_core::strings::Strings;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Strings {
    /// Loads the English catalog compiled into the binary.
    pub(crate) fn english() -> Self {
        Self(kumochi_shell_core::strings::Strings::english())
    }
}
