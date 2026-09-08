//! Output plumbing shared by every command: the two output modes (human and
//! `--json`), terminal-aware semantic styling, aligned tables, and short-id
//! rendering.

use std::io::IsTerminal;

use arena0_client::protocol::{ColorDepth, Slot, View};
use arena0_client::sanitize;
use owo_colors::{OwoColorize, Style as OwoStyle};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style as TuiStyle};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The output mode chosen once by the global `--json` flag and honored everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Aligned tables, short ids, color when the terminal supports it.
    Human,
    /// One JSON document per invocation, ids as full hex strings, no color.
    Json,
}

impl Mode {
    #[must_use]
    pub(crate) fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

/// Semantic ANSI styling shared by the full-screen workspace and run views.
///
/// Colors always use the terminal's default background. An explicit
/// `ARENA0_THEME=dark|light` wins; otherwise `COLORFGBG` supplies a best-effort
/// background hint. `NO_COLOR` disables colors while retaining text
/// attributes that distinguish important states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TuiPalette {
    enabled: bool,
    theme: ColorTheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorTheme {
    Dark,
    Light,
}

impl TuiPalette {
    #[must_use]
    pub(crate) fn detect() -> Self {
        Self {
            enabled: std::env::var_os("NO_COLOR").is_none(),
            theme: ColorTheme::from_hints(
                std::env::var("ARENA0_THEME").ok().as_deref(),
                std::env::var("COLORFGBG").ok().as_deref(),
            ),
        }
    }

    #[must_use]
    pub(crate) fn strong(self) -> TuiStyle {
        self.colored(
            match self.theme {
                ColorTheme::Dark => Color::LightBlue,
                ColorTheme::Light => Color::Blue,
            },
            Modifier::BOLD,
        )
    }

    #[must_use]
    pub(crate) fn emphasis(self) -> TuiStyle {
        self.colored(
            match self.theme {
                ColorTheme::Dark => Color::LightMagenta,
                ColorTheme::Light => Color::Magenta,
            },
            Modifier::BOLD,
        )
    }

    #[must_use]
    pub(crate) fn muted(self) -> TuiStyle {
        if self.enabled {
            TuiStyle::default().fg(match self.theme {
                ColorTheme::Dark => Color::Gray,
                ColorTheme::Light => Color::DarkGray,
            })
        } else {
            TuiStyle::default()
        }
    }

    #[must_use]
    pub(crate) fn success(self) -> TuiStyle {
        self.colored(
            match self.theme {
                ColorTheme::Dark => Color::LightGreen,
                ColorTheme::Light => Color::Green,
            },
            Modifier::BOLD,
        )
    }

    #[must_use]
    pub(crate) fn input(self) -> TuiStyle {
        self.colored(
            match self.theme {
                ColorTheme::Dark => Color::LightYellow,
                ColorTheme::Light => Color::Rgb(128, 80, 0),
            },
            Modifier::BOLD,
        )
    }

    #[must_use]
    pub(crate) fn public(self) -> TuiStyle {
        self.colored(
            match self.theme {
                ColorTheme::Dark => Color::LightCyan,
                ColorTheme::Light => Color::Cyan,
            },
            Modifier::BOLD,
        )
    }

    #[must_use]
    pub(crate) fn error(self) -> TuiStyle {
        self.colored(
            match self.theme {
                ColorTheme::Dark => Color::LightRed,
                ColorTheme::Light => Color::Red,
            },
            Modifier::BOLD,
        )
    }

    fn colored(self, color: Color, modifier: Modifier) -> TuiStyle {
        let style = TuiStyle::default().add_modifier(modifier);
        if self.enabled { style.fg(color) } else { style }
    }

    #[cfg(test)]
    const fn for_test(enabled: bool, theme: ColorTheme) -> Self {
        Self { enabled, theme }
    }
}

impl ColorTheme {
    fn from_hints(explicit: Option<&str>, colorfgbg: Option<&str>) -> Self {
        match explicit
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("light") => return Self::Light,
            Some("dark") => return Self::Dark,
            _ => {}
        }
        let background = colorfgbg
            .and_then(|value| value.rsplit(';').next())
            .and_then(|value| value.parse::<u8>().ok());
        match background {
            Some(7 | 9..=15) => Self::Light,
            _ => Self::Dark,
        }
    }
}

/// Semantic styles for durable non-TUI human output.
///
/// Every method is the identity function unless color is enabled, so JSON,
/// redirected output, and `NO_COLOR` never carry terminal styling.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Palette {
    enabled: bool,
}

impl Palette {
    /// Enable color only in human mode on a terminal stdout.
    #[must_use]
    pub(crate) fn for_mode(mode: Mode) -> Self {
        Self::from_capabilities(
            mode,
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        )
    }

    /// Style transient stderr progress only when stderr supports it.
    #[must_use]
    pub(crate) fn for_stderr(mode: Mode) -> Self {
        Self::from_capabilities(
            mode,
            std::io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        )
    }

    const fn from_capabilities(mode: Mode, is_terminal: bool, no_color: bool) -> Self {
        Self {
            enabled: matches!(mode, Mode::Human) && is_terminal && !no_color,
        }
    }

    /// A palette that never colors, for asserting on visible text in tests.
    #[cfg(test)]
    pub(crate) fn plain() -> Self {
        Self { enabled: false }
    }

    /// The color capability to request from program-authored views.
    #[must_use]
    pub(crate) fn color_depth(self) -> ColorDepth {
        if self.enabled {
            ColorDepth::Ansi16
        } else {
            ColorDepth::Mono
        }
    }

    fn apply(self, style: OwoStyle, s: &str) -> String {
        if self.enabled {
            s.style(style).to_string()
        } else {
            s.to_string()
        }
    }

    #[must_use]
    pub(crate) fn dim(self, s: &str) -> String {
        self.apply(OwoStyle::new().dimmed(), s)
    }
    #[must_use]
    pub(crate) fn bold(self, s: &str) -> String {
        self.apply(OwoStyle::new().bold(), s)
    }
    #[must_use]
    pub(crate) fn green(self, s: &str) -> String {
        self.apply(OwoStyle::new().green(), s)
    }
    #[must_use]
    pub(crate) fn red(self, s: &str) -> String {
        self.apply(OwoStyle::new().red(), s)
    }
    #[must_use]
    pub(crate) fn yellow(self, s: &str) -> String {
        self.apply(OwoStyle::new().yellow(), s)
    }
    #[must_use]
    pub(crate) fn cyan(self, s: &str) -> String {
        self.apply(OwoStyle::new().cyan(), s)
    }
}

/// The current terminal width for program views. Falls back to 80 columns when
/// stdout is not a terminal or the platform cannot report a width.
#[must_use]
pub(crate) fn terminal_width() -> u16 {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|w| *w > 0)
        .or_else(terminal_width_from_tty)
        .unwrap_or(80)
}

/// Center a bounded overlay while leaving a one-cell margin where possible.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// Render every program-owned slot for command-style views.
#[must_use]
pub(crate) fn render_view_summary(view: &View, palette: Palette) -> String {
    let mut out = String::new();
    if let Some(header) = slot_text(view, Slot::Header) {
        out.push_str(&palette.bold(header.trim_end()));
        out.push('\n');
    }
    if let Some(agents) = slot_text(view, Slot::Agents) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(agents.trim_end());
        out.push('\n');
    }
    if let Some(state) = slot_text(view, Slot::State) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(state.trim_end());
        out.push('\n');
    }
    if let Some(status) = slot_text(view, Slot::StatusBar) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&palette.dim(status.trim_end()));
        out.push('\n');
    }
    out
}

/// Render the compact slots used inline with watch step frames.
#[must_use]
pub(crate) fn render_view_inline(view: &View, palette: Palette) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(header) = inline_slot_text(view, Slot::Header) {
        parts.push(palette.bold(&header));
    }
    if let Some(status) = inline_slot_text(view, Slot::StatusBar) {
        parts.push(palette.dim(&status));
    }
    (!parts.is_empty()).then(|| parts.join("  "))
}

fn slot_text(view: &View, slot: Slot) -> Option<String> {
    view.slots
        .get(&slot)
        .map(|s| sanitize::sanitize(s))
        .filter(|s| !s.trim().is_empty())
}

fn inline_slot_text(view: &View, slot: Slot) -> Option<String> {
    slot_text(view, slot)
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
}

#[cfg(unix)]
fn terminal_width_from_tty() -> Option<u16> {
    use std::os::fd::AsRawFd;

    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return None;
    }
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // `ioctl(TIOCGWINSZ)` only writes the winsize struct for the current stdout fd.
    let rc = unsafe { libc::ioctl(stdout.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let size = unsafe { size.assume_init() };
    (size.ws_col > 0).then_some(size.ws_col)
}

#[cfg(not(unix))]
fn terminal_width_from_tty() -> Option<u16> {
    None
}

/// The visible width of a cell, ignoring ANSI escapes.
#[must_use]
pub(crate) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(sanitize::strip_ansi(s).as_str())
}

/// Render an aligned, left-justified table when it fits, or a stacked, wrapped
/// representation for a narrow terminal. The wide form puts a header row over
/// `rows`, with each column padded to its widest visible cell plus a two-space
/// gutter. The narrow form keeps every cell value (including copyable ids) and
/// wraps it to `width` columns instead of silently truncating it. Trailing
/// whitespace is trimmed per line. Cells may contain ANSI color; widths are
/// measured on the visible text so color never breaks alignment.
#[must_use]
pub(crate) fn render_table(
    headers: &[&str],
    rows: &[Vec<String>],
    palette: Palette,
    width: u16,
) -> String {
    let cols = headers.len();
    let mut widths = vec![0usize; cols];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = display_width(h);
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(display_width(cell));
        }
    }

    let width = usize::from(width.max(1));
    let table_width = widths
        .iter()
        .enumerate()
        .map(|(i, width)| width + usize::from(i > 0) * 2)
        .sum::<usize>();
    if table_width > width {
        return render_stacked_table(headers, rows, palette, width);
    }

    let mut out = String::new();
    let mut header_cells = Vec::with_capacity(cols);
    for (i, h) in headers.iter().enumerate() {
        header_cells.push(pad(&palette.dim(h), display_width(h), widths[i]));
    }
    out.push_str(header_cells.join("  ").trim_end());
    out.push('\n');

    for row in rows {
        let mut cells = Vec::with_capacity(cols);
        for (i, w) in widths.iter().enumerate() {
            let cell = row.get(i).map_or("", String::as_str);
            cells.push(pad(cell, display_width(cell), *w));
        }
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// Render one field per line while preserving all values on terminals too narrow
/// for the aligned table. ANSI styling is intentionally limited to the field
/// label; values are split from their sanitized visible text so an escape
/// sequence can never make a line exceed the requested width.
fn render_stacked_table(
    headers: &[&str],
    rows: &[Vec<String>],
    palette: Palette,
    width: usize,
) -> String {
    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.extend(
            wrap_chars("none", width)
                .into_iter()
                .map(|line| palette.dim(&line)),
        );
    }
    for (row_index, row) in rows.iter().enumerate() {
        if row_index > 0 {
            lines.push(String::new());
        }
        for (index, header) in headers.iter().enumerate() {
            let value = row.get(index).map_or("", String::as_str);
            let value = sanitize::strip_ansi(value).replace('\n', " ");
            let prefix = format!("{header}: ");
            let prefix_width = display_width(&prefix);

            if prefix_width < width {
                let chunks = wrap_chars(&value, width - prefix_width);
                if chunks.is_empty() {
                    lines.push(palette.dim(prefix.trim_end()));
                } else {
                    lines.push(format!(
                        "{}{}",
                        palette.dim(&prefix),
                        chunks.first().expect("non-empty chunks")
                    ));
                    let continuation_indent = " ".repeat(prefix_width);
                    for chunk in chunks.iter().skip(1) {
                        lines.push(format!("{continuation_indent}{chunk}"));
                    }
                }
            } else {
                // This only affects unusually tiny terminals. Keep the field
                // name and value rather than dropping either one, even when the
                // label itself needs to wrap.
                let field = format!("{header}: {value}");
                lines.extend(wrap_chars(&field, width));
            }
        }
    }

    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Split text at character boundaries without discarding any characters.
pub(crate) fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut chunk_width = 0usize;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if character_width > 0
            && chunk_width > 0
            && chunk_width.saturating_add(character_width) > width
        {
            chunks.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
        chunk.push(character);
        chunk_width = chunk_width.saturating_add(character_width);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Pad `cell` (whose visible width is `visible`) to `target` visible columns by
/// appending spaces.
fn pad(cell: &str, visible: usize, target: usize) -> String {
    let mut s = cell.to_string();
    if visible < target {
        s.push_str(&" ".repeat(target - visible));
    }
    s
}

/// Print a JSON document as the single result of a `--json` invocation.
pub(crate) fn print_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: encode json: {e}"),
    }
}

/// A compact single-line JSON rendering of a typed value (outcomes, contexts), for
/// inline display in human output.
#[must_use]
pub(crate) fn compact_json(value: &serde_json::Value) -> String {
    // A bare string renders without its surrounding quotes for readability.
    match value {
        serde_json::Value::String(s) => s.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_and_nonterminal_output_select_plain_styling() {
        assert!(!Palette::from_capabilities(Mode::Human, true, true).enabled);
        assert!(!Palette::from_capabilities(Mode::Human, false, false).enabled);
        assert!(!Palette::from_capabilities(Mode::Json, true, false).enabled);
        assert!(Palette::from_capabilities(Mode::Human, true, false).enabled);
    }

    #[test]
    fn tui_theme_hints_select_contrasting_semantic_palettes() {
        assert_eq!(
            ColorTheme::from_hints(Some("light"), None),
            ColorTheme::Light
        );
        assert_eq!(
            ColorTheme::from_hints(Some("dark"), Some("0;15")),
            ColorTheme::Dark
        );
        assert_eq!(
            ColorTheme::from_hints(None, Some("0;15")),
            ColorTheme::Light
        );
        assert_eq!(ColorTheme::from_hints(None, Some("15;0")), ColorTheme::Dark);

        let dark = TuiPalette::for_test(true, ColorTheme::Dark);
        assert_eq!(dark.strong().fg, Some(Color::LightBlue));
        assert_eq!(dark.emphasis().fg, Some(Color::LightMagenta));
        assert_eq!(dark.input().fg, Some(Color::LightYellow));
        assert_eq!(dark.public().fg, Some(Color::LightCyan));
        let light = TuiPalette::for_test(true, ColorTheme::Light);
        assert_eq!(light.strong().fg, Some(Color::Blue));
        assert_eq!(light.emphasis().fg, Some(Color::Magenta));
        assert_eq!(light.input().fg, Some(Color::Rgb(128, 80, 0)));
        assert_eq!(light.public().fg, Some(Color::Cyan));
        assert_eq!(
            TuiPalette::for_test(false, ColorTheme::Dark).strong().fg,
            None
        );
    }
}
