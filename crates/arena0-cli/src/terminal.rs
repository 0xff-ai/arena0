//! Shared ownership of the full-screen terminal lifecycle.

use std::sync::Once;

use anyhow::Context as _;
use crossterm::cursor::Show;

/// Enter the alternate screen and return the guard that restores it.
pub(crate) fn enter() -> anyhow::Result<(ratatui::DefaultTerminal, RestoreTerminal)> {
    install_cursor_panic_hook();
    let restore = RestoreTerminal;
    let terminal = ratatui::try_init().context("enter arena0 TUI")?;
    Ok((terminal, restore))
}

fn install_cursor_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = ratatui::try_restore();
            let _ = crossterm::execute!(std::io::stdout(), Show);
            previous(info);
        }));
    });
}

/// Restores raw mode, the alternate screen, attributes, and cursor visibility.
pub(crate) struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
        let _ = crossterm::execute!(std::io::stdout(), Show);
    }
}
