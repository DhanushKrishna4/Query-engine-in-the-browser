//! Byte-offset spans and the single diagnostic type used by every pipeline stage.
//!
//! Every token, AST node and bound expression carries a `Span` of byte offsets
//! into the original SQL text. That is what lets the lexer, parser and binder
//! all produce the same shape of error, and what lets the eventual browser UI
//! draw a squiggle under the exact characters that are wrong.

use std::fmt;

/// A half-open byte range `[start, end)` into the original SQL source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const EMPTY: Span = Span { start: 0, end: 0 };

    pub fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    /// Smallest span covering both inputs. Used to give a composite expression
    /// (`a + b`) a span running from the start of `a` to the end of `b`.
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Which pipeline stage produced a diagnostic. The UI colours errors by stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Lex,
    Parse,
    Bind,
    Execute,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Stage::Lex => "lex",
            Stage::Parse => "parse",
            Stage::Bind => "bind",
            Stage::Execute => "execute",
        };
        f.write_str(s)
    }
}

/// The one error type in the engine. Carrying `expected` and `hint` separately
/// (rather than baking them into `message`) keeps the structured form available
/// for the UI, which wants to render them differently from the message itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub stage: Stage,
    pub message: String,
    pub span: Option<Span>,
    /// For parse errors: the token kinds that would have been accepted here.
    pub expected: Vec<String>,
    /// A "did you mean ...?" style suggestion, rendered next to the caret.
    pub hint: Option<String>,
}

pub type Result<T> = std::result::Result<T, Diagnostic>;

impl Diagnostic {
    pub fn new(stage: Stage, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            stage,
            message: message.into(),
            span: None,
            expected: Vec::new(),
            hint: None,
        }
    }

    pub fn lex(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic::new(Stage::Lex, message).with_span(span)
    }

    pub fn parse(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic::new(Stage::Parse, message).with_span(span)
    }

    pub fn bind(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic::new(Stage::Bind, message).with_span(span)
    }

    pub fn exec(message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(Stage::Execute, message)
    }

    pub fn with_span(mut self, span: Span) -> Diagnostic {
        self.span = Some(span);
        self
    }

    pub fn with_expected<I, S>(mut self, expected: I) -> Diagnostic
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.expected = expected.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Diagnostic {
        self.hint = Some(hint.into());
        self
    }

    /// Single-line summary, e.g.
    /// `unexpected token `FROM`, expected one of [identifier, `(`]`
    pub fn headline(&self) -> String {
        if self.expected.is_empty() {
            self.message.clone()
        } else {
            format!(
                "{}, expected one of [{}]",
                self.message,
                self.expected.join(", ")
            )
        }
    }

    /// Render the diagnostic against the source text with a caret underline:
    ///
    /// ```text
    /// error: no such column `naem`
    ///   --> line 1, column 23
    ///    |
    ///  1 | SELECT id FROM people WHERE naem = 'x'
    ///    |                             ^^^^ did you mean `name`?
    /// ```
    pub fn render(&self, sql: &str) -> String {
        let mut out = format!("error[{}]: {}\n", self.stage, self.headline());

        let Some(span) = self.span else {
            return out;
        };

        let (line_idx, line_start) = line_of_offset(sql, span.start);
        let line_end = sql[line_start..]
            .find('\n')
            .map(|i| line_start + i)
            .unwrap_or(sql.len());
        let line_text = &sql[line_start..line_end];

        // Columns are 1-based and counted in characters, not bytes, so that a
        // caret under an identifier containing non-ASCII still lines up.
        let col = sql[line_start..span.start].chars().count() + 1;
        // A span can run past the end of its first line (a multi-line string
        // literal, say); clamp the underline to the line we are printing.
        let span_end = span.end.min(line_end).max(span.start);
        let width = sql[span.start..span_end].chars().count().max(1);

        let line_no = line_idx + 1;
        let gutter = line_no.to_string();
        let pad = " ".repeat(gutter.len());

        out.push_str(&format!("{pad} --> line {line_no}, column {col}\n"));
        out.push_str(&format!("{pad} |\n"));
        out.push_str(&format!("{gutter} | {line_text}\n"));
        out.push_str(&format!(
            "{pad} | {}{}",
            " ".repeat(col - 1),
            "^".repeat(width)
        ));
        if let Some(hint) = &self.hint {
            out.push_str(&format!(" {hint}"));
        }
        out.push('\n');
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.headline())
    }
}

impl std::error::Error for Diagnostic {}

/// Returns `(zero-based line index, byte offset of that line's first byte)`.
fn line_of_offset(sql: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(sql.len());
    let mut line = 0usize;
    let mut line_start = 0usize;
    for (i, b) in sql.as_bytes().iter().enumerate() {
        if i >= offset {
            break;
        }
        if *b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    (line, line_start)
}

/// Levenshtein distance, used to turn "no such column `naem`" into a
/// "did you mean `name`?" hint. Small inputs, so the simple DP is fine.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Pick the closest candidate to `name`, if any is close enough to be a likely
/// typo rather than a coincidence.
pub fn closest_match<'a, I: IntoIterator<Item = &'a str>>(name: &str, candidates: I) -> Option<String> {
    let lowered = name.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        let d = edit_distance(&lowered, &cand.to_ascii_lowercase());
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    // Allow roughly one edit per three characters before we stop guessing.
    best.filter(|(d, _)| *d <= (name.len() / 3).max(1) + 1)
        .map(|(_, c)| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_points_at_the_right_characters() {
        let sql = "SELECT id FROM people WHERE naem = 'x'";
        let d = Diagnostic::bind("no such column `naem`", Span::new(28, 32))
            .with_hint("did you mean `name`?");
        let rendered = d.render(sql);
        assert!(rendered.contains("line 1, column 29"), "{rendered}");
        let caret_line = rendered.lines().last().unwrap();
        assert!(caret_line.contains("^^^^"), "{rendered}");
        // The caret must sit directly beneath `naem`.
        let src_line = rendered.lines().nth(3).unwrap();
        let caret_col = caret_line.find('^').unwrap();
        assert_eq!(&src_line[caret_col..caret_col + 4], "naem");
    }

    #[test]
    fn reports_line_numbers_on_multiline_sql() {
        let sql = "SELECT id\nFROM people\nWHERE oops";
        let d = Diagnostic::bind("no such column `oops`", Span::new(28, 32));
        let rendered = d.render(sql);
        assert!(rendered.contains("line 3, column 7"), "{rendered}");
    }

    #[test]
    fn suggests_close_names_only() {
        assert_eq!(
            closest_match("naem", ["name", "id"]).as_deref(),
            Some("name")
        );
        assert_eq!(closest_match("zzzzzzzz", ["name", "id"]), None);
    }
}
