//! Hand-written lexer.
//!
//! Operates over the raw bytes of the source so that every token can record an
//! exact byte offset. Any byte >= 0x80 is treated as an identifier character,
//! which lets non-ASCII identifiers and string contents pass through unharmed
//! while keeping the scanner a simple byte loop.

pub mod token;

pub use token::{Keyword, Token, TokenKind};

use crate::error::{Diagnostic, Result, Span};

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

/// Tokenize an entire statement. The returned vector always ends with `Eof`,
/// so the parser can look ahead without bounds checks.
pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    Lexer::new(sql).tokenize()
}

impl<'a> Lexer<'a> {
    pub fn new(sql: &'a str) -> Lexer<'a> {
        Lexer {
            src: sql.as_bytes(),
            pos: 0,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            let tok = self.next_token()?;
            let is_eof = tok.kind == TokenKind::Eof;
            out.push(tok);
            if is_eof {
                return Ok(out);
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.src.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn text(&self, span: Span) -> String {
        String::from_utf8_lossy(&self.src[span.start..span.end]).into_owned()
    }

    fn next_token(&mut self) -> Result<Token> {
        self.skip_trivia()?;
        let start = self.pos;

        let Some(b) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: Span::new(start, start),
                text: String::new(),
            });
        };

        // A digit, or a `.` immediately followed by a digit, starts a number.
        // The second case is what distinguishes `.5` from the `.` in `t.col`.
        if b.is_ascii_digit() || (b == b'.' && self.peek_at(1).is_some_and(|c| c.is_ascii_digit())) {
            return self.lex_number(start);
        }

        if is_ident_start(b) {
            return Ok(self.lex_word(start));
        }

        match b {
            b'\'' => self.lex_string(start),
            b'"' => self.lex_quoted_ident(start),
            _ => self.lex_operator(start),
        }
    }

    /// Whitespace, `-- line comments` and `/* block comments */`.
    /// Block comments nest, as the SQL standard requires.
    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.pos += 1;
                }
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut depth = 1usize;
                    while depth > 0 {
                        match (self.peek(), self.peek_at(1)) {
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                self.pos += 2;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                self.pos += 2;
                            }
                            (Some(_), _) => self.pos += 1,
                            (None, _) => {
                                return Err(Diagnostic::lex(
                                    "unterminated block comment",
                                    Span::new(start, self.src.len()),
                                ))
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn lex_word(&mut self, start: usize) -> Token {
        while self.peek().is_some_and(is_ident_continue) {
            self.pos += 1;
        }
        let span = Span::new(start, self.pos);
        let text = self.text(span);
        let kind = match Keyword::lookup(&text) {
            Some(kw) => TokenKind::Keyword(kw),
            None => TokenKind::Ident,
        };
        Token { kind, span, text }
    }

    /// `'...'`, with a doubled quote (`''`) standing for a literal quote.
    fn lex_string(&mut self, start: usize) -> Result<Token> {
        self.pos += 1; // opening quote
        let mut value = Vec::new();
        loop {
            match self.bump() {
                Some(b'\'') => {
                    if self.peek() == Some(b'\'') {
                        value.push(b'\'');
                        self.pos += 1;
                    } else {
                        let span = Span::new(start, self.pos);
                        return Ok(Token {
                            kind: TokenKind::String,
                            span,
                            text: String::from_utf8_lossy(&value).into_owned(),
                        });
                    }
                }
                Some(b) => value.push(b),
                None => {
                    return Err(Diagnostic::lex(
                        "unterminated string literal",
                        Span::new(start, self.src.len()),
                    ))
                }
            }
        }
    }

    /// `"..."`, with a doubled quote (`""`) standing for a literal quote.
    /// Quoted identifiers keep their case and are never matched against the
    /// keyword table, so `"select"` is a perfectly good column name.
    fn lex_quoted_ident(&mut self, start: usize) -> Result<Token> {
        self.pos += 1;
        let mut value = Vec::new();
        loop {
            match self.bump() {
                Some(b'"') => {
                    if self.peek() == Some(b'"') {
                        value.push(b'"');
                        self.pos += 1;
                    } else {
                        let span = Span::new(start, self.pos);
                        if value.is_empty() {
                            return Err(Diagnostic::lex("empty quoted identifier", span));
                        }
                        return Ok(Token {
                            kind: TokenKind::QuotedIdent,
                            span,
                            text: String::from_utf8_lossy(&value).into_owned(),
                        });
                    }
                }
                Some(b) => value.push(b),
                None => {
                    return Err(Diagnostic::lex(
                        "unterminated quoted identifier",
                        Span::new(start, self.src.len()),
                    ))
                }
            }
        }
    }

    /// Integer, decimal or scientific-notation literal. The sign is *not* part
    /// of the token: `-1` lexes as `Minus` then `Number` and becomes a unary
    /// negation in the parser, which is what keeps `a-1` from lexing as
    /// `a` followed by the number `-1`.
    fn lex_number(&mut self, start: usize) -> Result<Token> {
        let mut seen_digit = false;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
            seen_digit = true;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
                seen_digit = true;
            }
        }
        if seen_digit && matches!(self.peek(), Some(b'e') | Some(b'E')) {
            // Only commit to an exponent if it is well formed; otherwise `1e`
            // is the number `1` followed by the identifier `e`, and the parser
            // will produce the better error.
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
            } else {
                self.pos = save;
            }
        }

        let span = Span::new(start, self.pos);
        // `1abc` is a malformed number rather than `1` followed by `abc`;
        // catching it here gives a caret on the whole thing.
        if self.peek().is_some_and(is_ident_start) {
            while self.peek().is_some_and(is_ident_continue) {
                self.pos += 1;
            }
            let span = Span::new(start, self.pos);
            let text = self.text(span);
            return Err(Diagnostic::lex(
                format!("malformed numeric literal `{text}`"),
                span,
            ));
        }
        Ok(Token {
            kind: TokenKind::Number,
            span,
            text: self.text(span),
        })
    }

    fn lex_operator(&mut self, start: usize) -> Result<Token> {
        let b = self.bump().expect("caller checked");
        let kind = match b {
            b'(' => TokenKind::LParen,
            b')' => TokenKind::RParen,
            b',' => TokenKind::Comma,
            b';' => TokenKind::Semicolon,
            b'.' => TokenKind::Period,
            b'+' => TokenKind::Plus,
            b'-' => TokenKind::Minus,
            b'*' => TokenKind::Star,
            b'/' => TokenKind::Slash,
            b'%' => TokenKind::Percent,
            b'=' => TokenKind::Eq,
            b'|' if self.peek() == Some(b'|') => {
                self.pos += 1;
                TokenKind::Concat
            }
            b'!' if self.peek() == Some(b'=') => {
                self.pos += 1;
                TokenKind::NotEq
            }
            b'<' => match self.peek() {
                Some(b'>') => {
                    self.pos += 1;
                    TokenKind::NotEq
                }
                Some(b'=') => {
                    self.pos += 1;
                    TokenKind::LtEq
                }
                _ => TokenKind::Lt,
            },
            b'>' => match self.peek() {
                Some(b'=') => {
                    self.pos += 1;
                    TokenKind::GtEq
                }
                _ => TokenKind::Gt,
            },
            other => {
                let span = Span::new(start, self.pos);
                return Err(Diagnostic::lex(
                    format!("unexpected character `{}`", other as char),
                    span,
                ));
            }
        };
        let span = Span::new(start, self.pos);
        Ok(Token {
            kind,
            span,
            text: self.text(span),
        })
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit() || b == b'$'
}

/// One line per token: `kind` `span` `text`. This is exactly what the browser
/// UI's TOKENS tab renders, so it lives next to the lexer rather than in the CLI.
pub fn format_tokens(tokens: &[Token]) -> String {
    let mut out = String::new();
    for t in tokens {
        if t.kind == TokenKind::Eof {
            continue;
        }
        let kind = match &t.kind {
            TokenKind::Keyword(k) => format!("Keyword({})", k.as_str()),
            other => format!("{other:?}"),
        };
        out.push_str(&format!(
            "{:<20} {:>4}..{:<4} {}\n",
            kind, t.span.start, t.span.end, t.text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokenize(sql).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            kinds("select SeLeCt"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn quoted_identifiers_keep_case_and_are_never_keywords() {
        let toks = tokenize(r#""Select" "a""b""#).unwrap();
        assert_eq!(toks[0].kind, TokenKind::QuotedIdent);
        assert_eq!(toks[0].text, "Select");
        assert_eq!(toks[1].text, r#"a"b"#);
    }

    #[test]
    fn spans_point_at_the_exact_bytes() {
        let sql = "SELECT id FROM t";
        let toks = tokenize(sql).unwrap();
        assert_eq!(&sql[toks[1].span.start..toks[1].span.end], "id");
        assert_eq!(&sql[toks[3].span.start..toks[3].span.end], "t");
    }

    #[test]
    fn string_literals_unescape_doubled_quotes() {
        let toks = tokenize("'it''s'").unwrap();
        assert_eq!(toks[0].kind, TokenKind::String);
        assert_eq!(toks[0].text, "it's");
    }

    #[test]
    fn numbers() {
        assert_eq!(kinds("1")[0], TokenKind::Number);
        assert_eq!(kinds("1.5")[0], TokenKind::Number);
        assert_eq!(kinds(".5")[0], TokenKind::Number);
        assert_eq!(kinds("1e10")[0], TokenKind::Number);
        assert_eq!(kinds("1.5E-3")[0], TokenKind::Number);
        // `1e` is not an exponent, so it splits into a number and a word.
        assert_eq!(
            kinds("1 e"),
            vec![TokenKind::Number, TokenKind::Ident, TokenKind::Eof]
        );
        // A dot after an identifier is member access, not a decimal point.
        assert_eq!(
            kinds("t.a"),
            vec![TokenKind::Ident, TokenKind::Period, TokenKind::Ident, TokenKind::Eof]
        );
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds("<= >= <> != || = < >"),
            vec![
                TokenKind::LtEq, TokenKind::GtEq, TokenKind::NotEq, TokenKind::NotEq,
                TokenKind::Concat, TokenKind::Eq, TokenKind::Lt, TokenKind::Gt,
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn comments_are_skipped_and_block_comments_nest() {
        assert_eq!(kinds("1 -- trailing\n"), vec![TokenKind::Number, TokenKind::Eof]);
        assert_eq!(kinds("1 /* a /* b */ c */ 2").len(), 3);
    }

    #[test]
    fn lex_errors_carry_a_span() {
        let e = tokenize("'unterminated").unwrap_err();
        assert!(e.message.contains("unterminated string"));
        assert_eq!(e.span.unwrap().start, 0);

        let e = tokenize("/* nope").unwrap_err();
        assert!(e.message.contains("unterminated block comment"));

        let e = tokenize("SELECT 1abc").unwrap_err();
        assert!(e.message.contains("malformed numeric literal"), "{}", e.message);

        let e = tokenize("SELECT #").unwrap_err();
        assert!(e.message.contains("unexpected character"));
    }

    #[test]
    fn non_ascii_identifiers_lex_as_one_token() {
        let toks = tokenize("SELECT café").unwrap();
        assert_eq!(toks[1].kind, TokenKind::Ident);
        assert_eq!(toks[1].text, "café");
    }
}
