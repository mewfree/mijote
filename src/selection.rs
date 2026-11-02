//! Mouse text selection over the rendered transcript. Pure text-range logic:
//! positions are (line, display column) in transcript content coordinates, so
//! a selection stays attached to its text while the view scrolls.

use unicode_width::UnicodeWidthChar;

pub type Pos = (usize, u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub head: Pos,
}

impl Selection {
    pub fn new(at: Pos) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// (start, end) in reading order; `end` is inclusive.
    fn ordered(&self) -> (Pos, Pos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Selected display columns `[from, to)` of line `line`, whose text is
    /// `width` columns wide. `None` when the line is untouched.
    pub fn cols_on(&self, line: usize, width: u16) -> Option<(u16, u16)> {
        let ((l0, c0), (l1, c1)) = self.ordered();
        if line < l0 || line > l1 {
            return None;
        }
        let from = if line == l0 { c0 } else { 0 };
        let to = if line == l1 {
            c1.saturating_add(1)
        } else {
            width
        };
        let to = to.min(width);
        (from < to).then_some((from, to))
    }

    /// Like `cols_on`, but never reaches into the left gutter (indent and
    /// the `▎`/`┊` bars), which is decoration rather than content.
    pub fn content_cols_on(&self, line: usize, text: &str) -> Option<(u16, u16)> {
        let (from, to) = self.cols_on(line, text_width(text))?;
        let from = from.max(gutter_width(text));
        (from < to).then_some((from, to))
    }

    /// The selected text, one `\n` per line break, trailing blanks trimmed.
    pub fn extract(&self, lines: &[String]) -> String {
        let (start, end) = self.ordered();
        let mut out = Vec::new();
        for (i, text) in lines.iter().enumerate().take(end.0 + 1).skip(start.0) {
            let Some((from, to)) = self.content_cols_on(i, text) else {
                out.push(String::new());
                continue;
            };
            let mut col = 0u16;
            let mut piece = String::new();
            for c in text.chars() {
                let w = c.width().unwrap_or(0) as u16;
                if col >= from && col < to {
                    piece.push(c);
                }
                col += w;
            }
            out.push(piece.trim_end().to_string());
        }
        out.join("\n")
    }
}

/// Leading decoration columns of a rendered line: up to `GUTTER` columns of
/// blanks and bars. Code indentation past the gutter is content and kept.
fn gutter_width(s: &str) -> u16 {
    const GUTTER: u16 = 4;
    let mut col = 0;
    for c in s.chars() {
        if col >= GUTTER || !matches!(c, ' ' | '▎' | '┊') {
            break;
        }
        col += c.width().unwrap_or(0) as u16;
    }
    col
}

pub fn text_width(s: &str) -> u16 {
    s.chars().map(|c| c.width().unwrap_or(0) as u16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<String> {
        ["hello world", "", "second line  ", "end"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn single_line_is_inclusive_of_both_ends() {
        let s = Selection {
            anchor: (0, 6),
            head: (0, 10),
        };
        assert_eq!(s.extract(&lines()), "world");
    }

    #[test]
    fn backwards_drag_selects_the_same_text() {
        let s = Selection {
            anchor: (2, 5),
            head: (0, 6),
        };
        assert_eq!(s.extract(&lines()), "world\n\nsecond");
    }

    #[test]
    fn middle_lines_are_whole_and_trailing_blanks_trimmed() {
        let s = Selection {
            anchor: (0, 0),
            head: (3, 1),
        };
        assert_eq!(s.extract(&lines()), "hello world\n\nsecond line\nen");
    }

    #[test]
    fn wide_chars_count_two_columns() {
        let l = vec!["日本語abc".to_string()];
        let s = Selection {
            anchor: (0, 2),
            head: (0, 6),
        };
        assert_eq!(s.extract(&l), "本語a");
    }

    #[test]
    fn gutter_is_not_copied() {
        let l = vec!["    indented".to_string(), "  ▎ user text".to_string()];
        let s = Selection {
            anchor: (0, 0),
            head: (1, 12),
        };
        assert_eq!(s.extract(&l), "indented\nuser text");
    }

    #[test]
    fn cols_on_skips_lines_outside_the_range() {
        let s = Selection {
            anchor: (1, 2),
            head: (2, 3),
        };
        assert_eq!(s.cols_on(0, 10), None);
        assert_eq!(s.cols_on(1, 10), Some((2, 10)));
        assert_eq!(s.cols_on(2, 10), Some((0, 4)));
        assert_eq!(s.cols_on(3, 10), None);
    }
}
