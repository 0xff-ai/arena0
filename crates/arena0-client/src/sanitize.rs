/// Hard cap on any single slot of program text (64 KiB), applied before parsing.
pub const MAX_SLOT_BYTES: usize = 64 * 1024;

/// Sanitize program-authored text for terminal display. Keeps SGR color/style
/// sequences (`ESC [ ... m`) and newlines; strips every other escape sequence and
/// every other C0/C1 control. The output is safe to hand to a terminal or to an
/// ANSI-to-spans parser.
#[must_use]
pub fn sanitize(input: &str) -> String {
    scan(truncate_on_char_boundary(input, MAX_SLOT_BYTES), true)
}

/// Strip every escape sequence and C0/C1 control (including SGR and newlines),
/// leaving only printable text. For echoing untrusted user input back at them
/// (e.g. a rejected answer), where a kept `ESC [31m` would smuggle a color into
/// the CLI's own output.
#[must_use]
pub fn strip_ansi(input: &str) -> String {
    scan(input, false)
}

fn scan(input: &str, keep_sgr: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                if keep_sgr {
                    out.push(c);
                }
            }
            '\u{1b}' => strip_or_keep_escape(&mut chars, &mut out, keep_sgr),
            '\u{0}'..='\u{1f}' | '\u{7f}' => {}
            '\u{80}'..='\u{9f}' => {}
            _ => out.push(c),
        }
    }

    out
}

fn truncate_on_char_boundary(input: &str, max: usize) -> &str {
    if input.len() <= max {
        return input;
    }
    let mut end = max;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn strip_or_keep_escape<I>(chars: &mut std::iter::Peekable<I>, out: &mut String, keep_sgr: bool)
where
    I: Iterator<Item = char>,
{
    let Some(kind) = chars.next() else {
        return;
    };
    match kind {
        '[' => strip_or_keep_csi(chars, out, keep_sgr),
        ']' => strip_until_osc_terminator(chars),
        'P' | 'X' | '^' | '_' => strip_until_st(chars),
        _ => {}
    }
}

fn strip_or_keep_csi<I>(chars: &mut std::iter::Peekable<I>, out: &mut String, keep_sgr: bool)
where
    I: Iterator<Item = char>,
{
    let mut seq = String::from("\u{1b}[");
    while let Some(&c) = chars.peek() {
        if ('\u{30}'..='\u{3f}').contains(&c) {
            seq.push(c);
            chars.next();
        } else {
            break;
        }
    }
    while let Some(&c) = chars.peek() {
        if ('\u{20}'..='\u{2f}').contains(&c) {
            seq.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let Some(&final_byte) = chars.peek() else {
        return;
    };
    if !('\u{40}'..='\u{7e}').contains(&final_byte) {
        return;
    }
    seq.push(final_byte);
    chars.next();
    if keep_sgr && final_byte == 'm' {
        out.push_str(&seq);
    }
}

fn strip_until_osc_terminator<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while let Some(c) = chars.next() {
        if c == '\u{7}' {
            break;
        }
        if c == '\u{1b}' && matches!(chars.peek(), Some('\\')) {
            chars.next();
            break;
        }
    }
}

fn strip_until_st<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && matches!(chars.peek(), Some('\\')) {
            chars.next();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_text_preserves_only_sgr_and_newlines() {
        for (input, expected) in [
            ("a\x1b[1;31mred\x1b[0mb", "a\x1b[1;31mred\x1b[0mb"),
            ("a\x1b[2Jb", "ab"),
            ("\x1b[10;5Hx", "x"),
            ("a\x1b]0;window title\x07b", "ab"),
            ("a\x1b]0;t\x1b\\b", "ab"),
            ("a\tb\r\nc\x07d", "ab\ncd"),
            ("a\x7fb", "ab"),
            ("a\u{9b}b", "ab"),
            ("a\x1b", "a"),
            ("a\x1b[", "a"),
        ] {
            assert_eq!(sanitize(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn truncation_keeps_the_complete_prefix_at_a_multibyte_boundary() {
        let prefix = "a".repeat(MAX_SLOT_BYTES - 1);
        assert_eq!(sanitize(&format!("{prefix}éz")), prefix);
        let exact = format!("{}é", "a".repeat(MAX_SLOT_BYTES - 2));
        assert_eq!(sanitize(&format!("{exact}z")), exact);
    }

    #[test]
    fn plain_text_strips_all_controls_without_applying_the_slot_limit() {
        for (input, expected) in [
            ("a\x1b[1;31mred\x1b[0mb", "aredb"),
            ("\x1b[2Jx", "x"),
            ("a\x1b]0;t\x07b", "ab"),
            ("a\tb\n\rc\x07d", "abcd"),
            ("a\u{9b}b", "ab"),
        ] {
            assert_eq!(strip_ansi(input), expected, "input: {input:?}");
        }
        let input = format!("{}\x1b[31mx\x1b[0m", "a".repeat(MAX_SLOT_BYTES));
        assert_eq!(strip_ansi(&input).chars().count(), MAX_SLOT_BYTES + 1);
    }
}
