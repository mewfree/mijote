//! Prompt editor: a text buffer with a cursor and a vim-style normal mode.
//! No terminal or rendering code here; the app maps keys onto these methods.
//!
//! Normal mode covers motions (`h l j k 0 ^ $ w b e gg G`), operators
//! (`d c y` + motion, doubled for whole lines), `x X D C s S r`, `i a I A o O`,
//! `p P`, counts, and undo/redo. Not covered: text objects, `f`/`t`, dot repeat,
//! named registers, visual mode.

#[derive(Clone)]
struct Snapshot {
    buf: Vec<char>,
    cur: usize,
}

#[derive(Clone)]
struct Register {
    text: Vec<char>,
    linewise: bool,
}

/// How a motion's range treats its endpoints.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Up to, not including, the target.
    Excl,
    /// Including the target.
    Incl,
    /// Whole lines from the cursor's to the target's.
    Lines,
}

const UNDO_LIMIT: usize = 100;

#[derive(Default)]
pub struct Editor {
    buf: Vec<char>,
    /// Char index into `buf`; may equal `buf.len()` (insert mode, empty buffer).
    cur: usize,
    reg: Option<Register>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Operator waiting for its motion: `d`, `c` or `y`.
    op: Option<char>,
    /// Typed count; 0 means none.
    count: usize,
    /// First key of a two-key command: `g` or `r`.
    pending: Option<char>,
    /// Column to return to when moving vertically through shorter lines.
    want_col: Option<usize>,
}

fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

impl Editor {
    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.buf.iter().filter(|&&c| c == '\n').count() + 1
    }

    /// Cursor as (row, column in chars).
    pub fn cursor_pos(&self) -> (usize, usize) {
        let row = self.buf[..self.cur].iter().filter(|&&c| c == '\n').count();
        (row, self.cur - self.line_start(self.cur))
    }

    /// Replace the contents, cursor at the end. Forgets undo history.
    pub fn set_text(&mut self, s: &str) {
        self.buf = s.chars().collect();
        self.cur = self.buf.len();
        self.reset_history();
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// Pending normal-mode keys (count, operator), for the status bar.
    pub fn pending(&self) -> String {
        let mut s = String::new();
        if self.count > 0 {
            s.push_str(&self.count.to_string());
        }
        s.extend(self.op);
        s.extend(self.pending);
        s
    }

    /// Drop a half-typed normal-mode command.
    pub fn cancel_pending(&mut self) {
        self.op = None;
        self.count = 0;
        self.pending = None;
    }

    // ---- insert mode -------------------------------------------------

    pub fn insert(&mut self, c: char) {
        self.want_col = None;
        self.buf.insert(self.cur, c);
        self.cur += 1;
    }

    pub fn backspace(&mut self) {
        self.want_col = None;
        if self.cur > 0 {
            self.cur -= 1;
            self.buf.remove(self.cur);
        }
    }

    pub fn delete_forward(&mut self) {
        if self.cur < self.buf.len() {
            self.buf.remove(self.cur);
        }
    }

    /// Ctrl-w: delete the word before the cursor.
    pub fn delete_word_back(&mut self) {
        let t = self.word_back(self.cur);
        self.buf.drain(t..self.cur);
        self.cur = t;
    }

    /// Ctrl-u: delete from the start of the line to the cursor.
    pub fn kill_line_back(&mut self) {
        let ls = self.line_start(self.cur);
        self.buf.drain(ls..self.cur);
        self.cur = ls;
    }

    pub fn left(&mut self) {
        self.want_col = None;
        if self.cur > self.line_start(self.cur) {
            self.cur -= 1;
        }
    }

    pub fn right(&mut self) {
        self.want_col = None;
        if self.cur < self.line_end(self.cur) {
            self.cur += 1;
        }
    }

    pub fn up(&mut self) {
        self.move_vertical(-1);
    }

    pub fn down(&mut self) {
        self.move_vertical(1);
    }

    pub fn home(&mut self) {
        self.want_col = None;
        self.cur = self.line_start(self.cur);
    }

    pub fn end(&mut self) {
        self.want_col = None;
        self.cur = self.line_end(self.cur);
    }

    fn move_vertical(&mut self, delta: isize) {
        if let Some(t) = self.vertical(delta) {
            self.cur = t;
        }
    }

    /// Call when leaving insert mode: the normal-mode cursor sits on a char.
    pub fn leave_insert(&mut self) {
        self.want_col = None;
        if self.cur > self.line_start(self.cur) {
            self.cur -= 1;
        }
    }

    // ---- undo ----------------------------------------------------------

    /// Remember the current state as one undo step. `normal` does this itself
    /// before every change; call it when entering insert mode some other way.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(s);
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(s);
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            buf: self.buf.clone(),
            cur: self.cur,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.buf = s.buf;
        self.cur = s.cur;
        self.clamp();
    }

    fn reset_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.cancel_pending();
        self.want_col = None;
    }

    // ---- geometry --------------------------------------------------------

    fn line_start(&self, i: usize) -> usize {
        self.buf[..i]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |p| p + 1)
    }

    /// Index of the line's `\n`, or the buffer length for the last line.
    fn line_end(&self, i: usize) -> usize {
        self.buf[i..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.buf.len(), |p| i + p)
    }

    fn first_nonblank(&self, i: usize) -> usize {
        let ls = self.line_start(i);
        let le = self.line_end(i);
        (ls..le)
            .find(|&j| !matches!(self.buf[j], ' ' | '\t'))
            .unwrap_or(le)
    }

    /// Normal mode keeps the cursor on a character, not past the line's end.
    fn clamp(&mut self) {
        self.cur = self.cur.min(self.buf.len());
        if self.cur > self.line_start(self.cur) && self.cur == self.line_end(self.cur) {
            self.cur -= 1;
        }
    }

    fn word_fwd(&self, mut i: usize, stop_at_eol: bool) -> usize {
        let len = self.buf.len();
        if i >= len {
            return len;
        }
        let k = class(self.buf[i]);
        if k != 0 {
            while i < len && class(self.buf[i]) == k {
                i += 1;
            }
        }
        while i < len && class(self.buf[i]) == 0 {
            if stop_at_eol && self.buf[i] == '\n' {
                break;
            }
            i += 1;
        }
        i
    }

    fn word_back(&self, mut i: usize) -> usize {
        while i > 0 && class(self.buf[i - 1]) == 0 {
            i -= 1;
        }
        if i > 0 {
            let k = class(self.buf[i - 1]);
            while i > 0 && class(self.buf[i - 1]) == k {
                i -= 1;
            }
        }
        i
    }

    /// Last char of the next word end after `i`.
    fn word_end(&self, mut i: usize) -> usize {
        let len = self.buf.len();
        i += 1;
        while i < len && class(self.buf[i]) == 0 {
            i += 1;
        }
        if i >= len {
            return len.saturating_sub(1);
        }
        let k = class(self.buf[i]);
        while i + 1 < len && class(self.buf[i + 1]) == k {
            i += 1;
        }
        i
    }

    /// Start of the line `delta` lines away (clamped), column kept as close as
    /// the target line allows. `None` if the cursor cannot move at all.
    fn vertical(&mut self, delta: isize) -> Option<usize> {
        let col = self
            .want_col
            .unwrap_or(self.cur - self.line_start(self.cur));
        let mut start = self.line_start(self.cur);
        let mut moved = 0;
        for _ in 0..delta.unsigned_abs() {
            if delta > 0 {
                let le = self.line_end(start);
                if le >= self.buf.len() {
                    break;
                }
                start = le + 1;
            } else {
                if start == 0 {
                    break;
                }
                start = self.line_start(start - 1);
            }
            moved += 1;
        }
        if moved == 0 && delta != 0 {
            return None;
        }
        self.want_col = Some(col);
        Some(start + col.min(self.line_end(start) - start))
    }

    // ---- normal mode -----------------------------------------------------

    /// Feed one key in normal mode. Returns true when the command ends in
    /// insert mode (`i`, `cw`, `o`, ...).
    pub fn normal(&mut self, c: char) -> bool {
        if !matches!(c, 'j' | 'k') {
            self.want_col = None;
        }
        let insert = self.normal_key(c);
        if !insert {
            self.clamp();
        }
        insert
    }

    fn normal_key(&mut self, c: char) -> bool {
        if let Some(p) = self.pending.take() {
            return self.second_key(p, c);
        }
        if c.is_ascii_digit() && (c != '0' || self.count > 0) {
            self.count = (self.count * 10 + (c as usize - '0' as usize)).min(9999);
            return false;
        }
        let (op, c) = match (self.op, c) {
            (None, 'x') => (Some('d'), 'l'),
            (None, 'X') => (Some('d'), 'h'),
            (None, 'D') => (Some('d'), '$'),
            (None, 'C') => (Some('c'), '$'),
            (None, 's') => (Some('c'), 'l'),
            (None, 'S') => (Some('c'), 'c'),
            (op, c) => (op, c),
        };
        if op.is_none() && matches!(c, 'd' | 'c' | 'y') {
            self.op = Some(c);
            return false;
        }
        if c == 'g' || (c == 'r' && op.is_none()) {
            self.pending = Some(c);
            return false;
        }
        let n = self.count.max(1);
        self.count = 0;
        self.op = None;
        self.run(op, c, n)
    }

    /// `gg` and `r<char>`.
    fn second_key(&mut self, first: char, c: char) -> bool {
        let n = self.count.max(1);
        let op = self.op.take();
        self.count = 0;
        match (first, c) {
            ('g', 'g') => {
                let t = self.first_nonblank(0);
                self.apply(op, t, Kind::Lines)
            }
            ('r', ch) => {
                let end = self.cur + n;
                if end <= self.line_end(self.cur) {
                    self.checkpoint();
                    self.buf[self.cur..end].fill(ch);
                    self.cur = end - 1;
                }
                false
            }
            _ => false,
        }
    }

    fn run(&mut self, op: Option<char>, c: char, n: usize) -> bool {
        // dd, cc, yy: the operator doubled works on whole lines.
        if let Some(o) = op.filter(|&o| o == c) {
            let t = if n > 1 {
                self.vertical(n as isize - 1).unwrap_or(self.cur)
            } else {
                self.cur
            };
            return self.operate(o, t, Kind::Lines);
        }
        if let Some((t, kind)) = self.motion(c, n, op) {
            return self.apply(op, t, kind);
        }
        if op.is_some() {
            return false;
        }
        match c {
            'i' => {
                self.checkpoint();
                true
            }
            'a' => {
                self.checkpoint();
                if self.cur < self.line_end(self.cur) {
                    self.cur += 1;
                }
                true
            }
            'A' => {
                self.checkpoint();
                self.cur = self.line_end(self.cur);
                true
            }
            'I' => {
                self.checkpoint();
                self.cur = self.first_nonblank(self.cur);
                true
            }
            'o' => {
                self.checkpoint();
                let e = self.line_end(self.cur);
                self.buf.insert(e, '\n');
                self.cur = e + 1;
                true
            }
            'O' => {
                self.checkpoint();
                let s = self.line_start(self.cur);
                self.buf.insert(s, '\n');
                self.cur = s;
                true
            }
            'p' => self.paste(true, n),
            'P' => self.paste(false, n),
            'u' => {
                self.undo();
                false
            }
            _ => false,
        }
    }

    /// Where `c` takes the cursor from here, if it is a motion.
    fn motion(&mut self, c: char, n: usize, op: Option<char>) -> Option<(usize, Kind)> {
        let cur = self.cur;
        Some(match c {
            'h' => (cur.saturating_sub(n).max(self.line_start(cur)), Kind::Excl),
            'l' => ((cur + n).min(self.line_end(cur)), Kind::Excl),
            '0' => (self.line_start(cur), Kind::Excl),
            '^' => (self.first_nonblank(cur), Kind::Excl),
            '$' => (self.line_end(cur), Kind::Excl),
            'w' if op == Some('c') && self.buf.get(cur).is_some_and(|&ch| class(ch) != 0) => {
                // `cw` changes to the end of the word, like `ce`.
                let mut i = cur;
                for k in 0..n {
                    let at_end = self
                        .buf
                        .get(i + 1)
                        .is_none_or(|&next| class(next) != class(self.buf[i]));
                    if !(k == 0 && at_end) {
                        i = self.word_end(i);
                    }
                }
                (i, Kind::Incl)
            }
            'w' => {
                // An operator stops at the end of the line instead of joining.
                let stop = op.is_some();
                ((0..n).fold(cur, |i, _| self.word_fwd(i, stop)), Kind::Excl)
            }
            'b' => ((0..n).fold(cur, |i, _| self.word_back(i)), Kind::Excl),
            'e' => ((0..n).fold(cur, |i, _| self.word_end(i)), Kind::Incl),
            'j' => (self.vertical(n as isize)?, Kind::Lines),
            'k' => (self.vertical(-(n as isize))?, Kind::Lines),
            'G' => {
                let last = self.line_start(self.buf.len());
                (self.first_nonblank(last), Kind::Lines)
            }
            _ => return None,
        })
    }

    /// Move the cursor, or run the pending operator over the motion's range.
    fn apply(&mut self, op: Option<char>, target: usize, kind: Kind) -> bool {
        match op {
            Some(o) => self.operate(o, target, kind),
            None => {
                self.cur = target;
                false
            }
        }
    }

    fn operate(&mut self, op: char, target: usize, kind: Kind) -> bool {
        let lo = self.cur.min(target);
        let hi = self.cur.max(target);
        let len = self.buf.len();
        let (start, end) = match kind {
            Kind::Excl => (lo, hi),
            Kind::Incl => (lo, (hi + 1).min(len)),
            Kind::Lines => (self.line_start(lo), self.line_end(hi)),
        };
        let linewise = kind == Kind::Lines;
        if start == end && !linewise {
            if op == 'c' {
                self.checkpoint();
            }
            return op == 'c';
        }
        self.reg = Some(Register {
            text: self.buf[start..end].to_vec(),
            linewise,
        });
        match op {
            'y' => {
                self.cur = lo;
                false
            }
            'c' => {
                self.checkpoint();
                self.buf.drain(start..end);
                self.cur = start;
                true
            }
            _ => {
                self.checkpoint();
                if linewise {
                    self.delete_lines(start, end);
                } else {
                    self.buf.drain(start..end);
                    self.cur = start;
                }
                false
            }
        }
    }

    /// Remove the lines spanning `start..end`, along with one newline so no
    /// blank line is left behind.
    fn delete_lines(&mut self, start: usize, end: usize) {
        let len = self.buf.len();
        let pos = if end < len {
            self.buf.drain(start..=end);
            start
        } else if start > 0 {
            self.buf.drain(start - 1..end);
            start - 1
        } else {
            self.buf.clear();
            0
        };
        self.cur = self.first_nonblank(self.line_start(pos));
    }

    fn paste(&mut self, after: bool, n: usize) -> bool {
        let Some(reg) = self.reg.clone() else {
            return false;
        };
        self.checkpoint();
        if reg.linewise {
            let mut ins = Vec::new();
            for _ in 0..n {
                if after {
                    ins.push('\n');
                    ins.extend(&reg.text);
                } else {
                    ins.extend(&reg.text);
                    ins.push('\n');
                }
            }
            let pos = if after {
                self.line_end(self.cur)
            } else {
                self.line_start(self.cur)
            };
            self.buf.splice(pos..pos, ins);
            self.cur = self.first_nonblank(if after { pos + 1 } else { pos });
        } else {
            let ins: Vec<char> = reg.text.repeat(n);
            let pos = if after && self.cur < self.line_end(self.cur) {
                self.cur + 1
            } else {
                self.cur
            };
            let last = pos + ins.len().saturating_sub(1);
            self.buf.splice(pos..pos, ins);
            self.cur = last;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(s: &str) -> Editor {
        let mut e = Editor::default();
        e.set_text(s);
        e.cur = 0;
        e
    }

    /// Feed normal-mode keys; returns whether the last one entered insert.
    fn keys(e: &mut Editor, ks: &str) -> bool {
        let mut ins = false;
        for c in ks.chars() {
            ins = e.normal(c);
            if ins {
                break;
            }
        }
        ins
    }

    #[test]
    fn dd_deletes_the_current_line() {
        let mut e = ed("one\ntwo\nthree");
        keys(&mut e, "jdd");
        assert_eq!(e.text(), "one\nthree");
        assert_eq!(e.cursor_pos(), (1, 0));
        keys(&mut e, "dd");
        assert_eq!(e.text(), "one");
        keys(&mut e, "dd");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn counts_and_word_motions() {
        let mut e = ed("a bb ccc dddd");
        keys(&mut e, "2w");
        assert_eq!(e.cur, 5);
        keys(&mut e, "b");
        assert_eq!(e.cur, 2);
        keys(&mut e, "e");
        assert_eq!(e.cur, 3);
        keys(&mut e, "$");
        assert_eq!(e.cur, 12);
        keys(&mut e, "0d2w");
        assert_eq!(e.text(), "ccc dddd");
    }

    #[test]
    fn dw_stops_at_end_of_line() {
        let mut e = ed("foo bar\nbaz");
        keys(&mut e, "wdw");
        assert_eq!(e.text(), "foo \nbaz");
    }

    #[test]
    fn cw_changes_the_word_and_enters_insert() {
        let mut e = ed("foo bar");
        assert!(keys(&mut e, "cw"));
        for c in "xy".chars() {
            e.insert(c);
        }
        e.leave_insert();
        assert_eq!(e.text(), "xy bar");
        assert_eq!(e.cur, 1);
    }

    #[test]
    fn x_d_and_c() {
        let mut e = ed("hello world");
        keys(&mut e, "3x");
        assert_eq!(e.text(), "lo world");
        keys(&mut e, "wD");
        assert_eq!(e.text(), "lo ");
        assert!(keys(&mut e, "C"));
        assert_eq!(e.text(), "lo");
    }

    #[test]
    fn yank_and_paste_lines_and_words() {
        let mut e = ed("a\nb");
        keys(&mut e, "yyjp");
        assert_eq!(e.text(), "a\nb\na");
        keys(&mut e, "ggP");
        assert_eq!(e.text(), "a\na\nb\na");

        let mut e = ed("foo bar");
        keys(&mut e, "yw$p");
        assert_eq!(e.text(), "foo barfoo ");
    }

    #[test]
    fn open_line_and_insert_commands() {
        let mut e = ed("a\nb");
        assert!(keys(&mut e, "o"));
        e.insert('x');
        assert_eq!(e.text(), "a\nx\nb");
        e.leave_insert();
        assert!(keys(&mut e, "O"));
        assert_eq!(e.text(), "a\n\nx\nb");
        e.leave_insert();
        assert!(keys(&mut e, "A"));
        e.insert('!');
        assert_eq!(e.text(), "a\n!\nx\nb");
    }

    #[test]
    fn undo_and_redo() {
        let mut e = ed("one\ntwo");
        keys(&mut e, "dd");
        assert_eq!(e.text(), "two");
        keys(&mut e, "u");
        assert_eq!(e.text(), "one\ntwo");
        e.redo();
        assert_eq!(e.text(), "two");
        // One insert session is one undo step.
        keys(&mut e, "i");
        e.insert('a');
        e.insert('b');
        e.leave_insert();
        keys(&mut e, "u");
        assert_eq!(e.text(), "two");
    }

    #[test]
    fn vertical_motion_remembers_the_column() {
        let mut e = ed("abcdef\nab\nabcdef");
        keys(&mut e, "$jj");
        assert_eq!(e.cursor_pos(), (2, 5));
    }

    #[test]
    fn replace_and_gg() {
        let mut e = ed("abc\ndef");
        keys(&mut e, "2rx");
        assert_eq!(e.text(), "xxc\ndef");
        keys(&mut e, "Gdgg");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn insert_editing_helpers() {
        let mut e = ed("");
        for c in "foo bar".chars() {
            e.insert(c);
        }
        e.delete_word_back();
        assert_eq!(e.text(), "foo ");
        e.backspace();
        e.kill_line_back();
        assert_eq!(e.text(), "");
        assert_eq!(e.line_count(), 1);
    }

    #[test]
    fn pending_shows_count_and_operator() {
        let mut e = ed("a");
        keys(&mut e, "2d");
        assert_eq!(e.pending(), "2d");
        e.cancel_pending();
        assert_eq!(e.pending(), "");
    }
}
