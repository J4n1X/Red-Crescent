//! The hostname example from ABI.md: `hostname.get()` returns the machine's
//! name, `hostname.print()` writes it into the page.

use red_crescent_plugin::prelude::*;

#[plugin]
mod hostname {
    use super::*;

    /// hostname.get() -> string
    #[function]
    fn get() -> String {
        gethostname::gethostname().to_string_lossy().into_owned()
    }

    /// hostname.print(): writes the name into the page, unescaped like print().
    #[function]
    fn print(call: &Call) -> Result<()> {
        call.write(get())
    }
}
