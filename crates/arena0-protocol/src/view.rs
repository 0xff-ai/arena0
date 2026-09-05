use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// The color capability of the terminal the view will render into.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub enum ColorDepth {
    Mono,
    Ansi16,
    Ansi256,
    TrueColor,
}

impl ColorDepth {
    /// Whether this color depth supports ANSI styling.
    #[must_use]
    pub const fn supports_color(self) -> bool {
        !matches!(self, Self::Mono)
    }
}

/// What a viewer's terminal can show, handed to the program so it can wrap or
/// clamp to `width` and honor `color`.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct Viewport {
    pub width: u16,
    pub color: ColorDepth,
}

impl Viewport {
    /// Fit multiline view text to the viewport width while preserving ANSI SGR
    /// sequences and resetting styling when truncation occurs.
    #[must_use]
    pub fn fit_text(&self, text: impl AsRef<str>) -> String {
        text.as_ref()
            .lines()
            .map(|line| self.fit_line(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fit_line(&self, line: &str) -> String {
        let width = usize::from(self.width);
        if width == 0 {
            return String::new();
        }

        let mut out = String::new();
        let mut visible = 0;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                out.push(ch);
                for sgr in chars.by_ref() {
                    out.push(sgr);
                    if sgr == 'm' {
                        break;
                    }
                }
                continue;
            }
            if visible == width {
                break;
            }
            out.push(ch);
            visible += 1;
        }
        if visible == width && line.contains("\x1b[") {
            out.push_str("\x1b[0m");
        }
        out
    }
}

/// The fixed set of slots a frontend composes into its presentation.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
pub enum Slot {
    Header,
    Agents,
    State,
    StatusBar,
}

/// A program-authored view: text per slot. Slot text is UTF-8 and may contain
/// ANSI SGR sequences (`CSI ... m`) only; the client sanitizes before display.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Default, PartialEq, Eq,
)]
pub struct View {
    pub slots: BTreeMap<Slot, String>,
}

impl View {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn header(mut self, s: impl Into<String>) -> Self {
        self.slots.insert(Slot::Header, s.into());
        self
    }

    #[must_use]
    pub fn agents(mut self, s: impl Into<String>) -> Self {
        self.slots.insert(Slot::Agents, s.into());
        self
    }

    #[must_use]
    pub fn state(mut self, s: impl Into<String>) -> Self {
        self.slots.insert(Slot::State, s.into());
        self
    }

    #[must_use]
    pub fn status_bar(mut self, s: impl Into<String>) -> Self {
        self.slots.insert(Slot::StatusBar, s.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_depth_reports_ansi_support() {
        assert!(!ColorDepth::Mono.supports_color());
        assert!(ColorDepth::Ansi16.supports_color());
        assert!(ColorDepth::Ansi256.supports_color());
        assert!(ColorDepth::TrueColor.supports_color());
    }

    #[test]
    fn viewport_fit_text_handles_width_unicode_and_sgr() {
        let viewport = Viewport {
            width: 3,
            color: ColorDepth::Mono,
        };

        assert_eq!(viewport.fit_text("abcdef"), "abc");
        assert_eq!(viewport.fit_text("é🙂xy"), "é🙂x");
        assert_eq!(viewport.fit_text("one\ntwo"), "one\ntwo");
        assert_eq!(viewport.fit_text("\x1b[31mabcdef"), "\x1b[31mabc\x1b[0m");
        assert_eq!(viewport.fit_text("\x1b[31"), "\x1b[31");
        assert_eq!(
            Viewport {
                width: 0,
                color: ColorDepth::Mono,
            }
            .fit_text("anything"),
            ""
        );
    }
}
