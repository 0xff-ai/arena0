use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::session::Participant;
use arena0_program::MAX_PHASE_NAME_BYTES;

/// The color capability of the terminal the view will render into.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Slot {
    Header,
    Agents,
    State,
    StatusBar,
}

/// A program-authored view: text per slot, plus optional typed blocks. Slot
/// text is UTF-8 and may contain ANSI SGR sequences (`CSI ... m`) only; the
/// client sanitizes before display.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Default, PartialEq, Eq,
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct View {
    pub slots: BTreeMap<Slot, String>,
    /// Typed blocks for rich clients. Text slots remain the portable
    /// rendering; clients that do not understand a block ignore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<_>", optional))]
    pub blocks: Vec<Block>,
    /// Index in the committed ensemble of the participant whose message the
    /// program accepts next, as its `on_message` checks it. `None` when no
    /// participant may send from this state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub turn: Option<u8>,
    /// The program's current phase name, for programs that declare phases.
    /// The SDK fills it from the declared phase; programs do not set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub phase: Option<String>,
}

/// One typed piece of a program view.
///
/// A block carries no information the text slots do not: it is the same view,
/// laid out for a client that can draw a board or a table.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Block {
    /// Labelled values, such as the reserve price or whose turn it is.
    Facts {
        title: Option<String>,
        items: Vec<Fact>,
    },
    /// Rows under named columns, such as bids or rounds.
    Table {
        title: Option<String>,
        columns: Vec<String>,
        rows: Vec<Vec<Cell>>,
    },
    /// A rectangular board in row-major order, such as a chess board. The
    /// label vectors are empty or hold one label per row and column.
    Board {
        title: Option<String>,
        rows: u8,
        cols: u8,
        cells: Vec<Cell>,
        row_labels: Vec<String>,
        col_labels: Vec<String>,
    },
    /// Progress toward a bound.
    Progress { label: String, value: u64, max: u64 },
    /// One entry per participant, such as scores or readiness.
    Roster {
        title: Option<String>,
        entries: Vec<RosterEntry>,
    },
}

/// A labelled value in a [`Block::Facts`].
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Fact {
    pub label: String,
    pub value: Cell,
}

/// One displayed value. `participant` ties the value to a participant index
/// in the committed ensemble so clients can colour it consistently.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Cell {
    pub text: String,
    #[serde(default)]
    pub tone: Tone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub participant: Option<u8>,
}

impl Cell {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: Tone::Normal,
            participant: None,
        }
    }

    #[must_use]
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    #[must_use]
    pub fn participant(mut self, index: u8) -> Self {
        self.participant = Some(index);
        self
    }
}

/// One participant's line in a [`Block::Roster`].
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RosterEntry {
    pub participant: u8,
    pub status: Cell,
    pub detail: Option<String>,
}

/// A semantic emphasis; clients choose the colours.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Tone {
    #[default]
    Normal,
    Muted,
    Good,
    Warn,
    Bad,
    Highlight,
}

/// Blocks a view may carry.
pub const MAX_BLOCKS: usize = 16;
/// Rows in one [`Block::Table`].
pub const MAX_TABLE_ROWS: usize = 64;
/// Columns in one [`Block::Table`], and so cells in one of its rows.
pub const MAX_TABLE_COLUMNS: usize = 16;
/// Rows, and separately columns, of one [`Block::Board`].
pub const MAX_BOARD_SIDE: u8 = 32;
/// Entries in one [`Block::Roster`].
pub const MAX_ROSTER_ENTRIES: usize = 64;
/// Bytes of any one string in a block.
pub const MAX_BLOCK_TEXT_BYTES: usize = 256;

/// A view that breaks a documented limit. Every variant names the block by
/// its position in [`View::blocks`] and the limit it broke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    #[error("view has {count} blocks; the limit is {MAX_BLOCKS}")]
    Blocks { count: usize },
    #[error("block {block}: table has {count} rows; the limit is {MAX_TABLE_ROWS}")]
    TableRows { block: usize, count: usize },
    #[error("block {block}: table has {count} columns; the limit is {MAX_TABLE_COLUMNS}")]
    TableColumns { block: usize, count: usize },
    #[error(
        "block {block}: board is {rows}x{cols}; the limit is {MAX_BOARD_SIDE}x{MAX_BOARD_SIDE}"
    )]
    BoardSize { block: usize, rows: u8, cols: u8 },
    #[error("block {block}: board {rows}x{cols} needs {expected} cells but has {actual}")]
    BoardCells {
        block: usize,
        rows: u8,
        cols: u8,
        expected: usize,
        actual: usize,
    },
    #[error("block {block}: board has {actual} {axis} labels; expected none or {expected}")]
    BoardLabels {
        block: usize,
        axis: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("block {block}: roster has {count} entries; the limit is {MAX_ROSTER_ENTRIES}")]
    RosterEntries { block: usize, count: usize },
    #[error("block {block}: text is {len} bytes; the limit is {MAX_BLOCK_TEXT_BYTES}")]
    Text { block: usize, len: usize },
    #[error("block {block}: participant {participant} is outside the ensemble of {participants}")]
    Participant {
        block: usize,
        participant: u8,
        participants: usize,
    },
    #[error("turn {participant} is outside the ensemble of {participants}")]
    Turn {
        participant: u8,
        participants: usize,
    },
    #[error("phase is {len} bytes; the limit is {MAX_PHASE_NAME_BYTES}")]
    Phase { len: usize },
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

    #[must_use]
    pub fn block(mut self, block: Block) -> Self {
        self.blocks.push(block);
        self
    }

    /// Name the participant whose message the program accepts next. Pass the
    /// same state helper `on_message` checks, so the view cannot disagree
    /// with the program's rules.
    #[must_use]
    pub fn turn(mut self, participant: Option<Participant>) -> Self {
        self.turn = participant.map(Participant::as_u8);
        self
    }

    /// Check the turn, phase and blocks against their limits for a session of
    /// `participants`.
    ///
    /// A guest authors the view, so the Host calls this on every view it
    /// decodes, before any client sees it. A violation rejects the whole view
    /// rather than truncating a block: a clipped board would mislead.
    pub fn validate(&self, participants: usize) -> Result<(), ViewError> {
        if let Some(participant) = self.turn
            && usize::from(participant) >= participants
        {
            return Err(ViewError::Turn {
                participant,
                participants,
            });
        }
        if let Some(phase) = &self.phase
            && phase.len() > MAX_PHASE_NAME_BYTES
        {
            return Err(ViewError::Phase { len: phase.len() });
        }
        if self.blocks.len() > MAX_BLOCKS {
            return Err(ViewError::Blocks {
                count: self.blocks.len(),
            });
        }
        for (index, block) in self.blocks.iter().enumerate() {
            Check {
                index,
                participants,
            }
            .block(block)?;
        }
        Ok(())
    }
}

/// The context of one block's validation: its position, for error messages,
/// and the size of the ensemble that participant indices must fall within.
struct Check {
    index: usize,
    participants: usize,
}

impl Check {
    fn block(&self, block: &Block) -> Result<(), ViewError> {
        match block {
            Block::Facts { title, items } => {
                self.optional_text(title.as_deref())?;
                for fact in items {
                    self.text(&fact.label)?;
                    self.cell(&fact.value)?;
                }
            }
            Block::Table {
                title,
                columns,
                rows,
            } => {
                self.optional_text(title.as_deref())?;
                if columns.len() > MAX_TABLE_COLUMNS {
                    return Err(ViewError::TableColumns {
                        block: self.index,
                        count: columns.len(),
                    });
                }
                if rows.len() > MAX_TABLE_ROWS {
                    return Err(ViewError::TableRows {
                        block: self.index,
                        count: rows.len(),
                    });
                }
                for column in columns {
                    self.text(column)?;
                }
                for row in rows {
                    // A row may not be wider than the widest table allowed.
                    if row.len() > MAX_TABLE_COLUMNS {
                        return Err(ViewError::TableColumns {
                            block: self.index,
                            count: row.len(),
                        });
                    }
                    for cell in row {
                        self.cell(cell)?;
                    }
                }
            }
            Block::Board {
                title,
                rows,
                cols,
                cells,
                row_labels,
                col_labels,
            } => {
                self.optional_text(title.as_deref())?;
                if *rows > MAX_BOARD_SIDE || *cols > MAX_BOARD_SIDE {
                    return Err(ViewError::BoardSize {
                        block: self.index,
                        rows: *rows,
                        cols: *cols,
                    });
                }
                let expected = usize::from(*rows) * usize::from(*cols);
                if cells.len() != expected {
                    return Err(ViewError::BoardCells {
                        block: self.index,
                        rows: *rows,
                        cols: *cols,
                        expected,
                        actual: cells.len(),
                    });
                }
                for (axis, expected, labels) in [
                    ("row", usize::from(*rows), row_labels),
                    ("column", usize::from(*cols), col_labels),
                ] {
                    if !labels.is_empty() && labels.len() != expected {
                        return Err(ViewError::BoardLabels {
                            block: self.index,
                            axis,
                            expected,
                            actual: labels.len(),
                        });
                    }
                    for label in labels {
                        self.text(label)?;
                    }
                }
                for cell in cells {
                    self.cell(cell)?;
                }
            }
            Block::Progress { label, .. } => self.text(label)?,
            Block::Roster { title, entries } => {
                self.optional_text(title.as_deref())?;
                if entries.len() > MAX_ROSTER_ENTRIES {
                    return Err(ViewError::RosterEntries {
                        block: self.index,
                        count: entries.len(),
                    });
                }
                for entry in entries {
                    self.participant(entry.participant)?;
                    self.cell(&entry.status)?;
                    self.optional_text(entry.detail.as_deref())?;
                }
            }
        }
        Ok(())
    }

    fn cell(&self, cell: &Cell) -> Result<(), ViewError> {
        self.text(&cell.text)?;
        cell.participant
            .map_or(Ok(()), |participant| self.participant(participant))
    }

    fn text(&self, text: &str) -> Result<(), ViewError> {
        if text.len() > MAX_BLOCK_TEXT_BYTES {
            return Err(ViewError::Text {
                block: self.index,
                len: text.len(),
            });
        }
        Ok(())
    }

    fn optional_text(&self, text: Option<&str>) -> Result<(), ViewError> {
        text.map_or(Ok(()), |text| self.text(text))
    }

    fn participant(&self, participant: u8) -> Result<(), ViewError> {
        if usize::from(participant) >= self.participants {
            return Err(ViewError::Participant {
                block: self.index,
                participant,
                participants: self.participants,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
