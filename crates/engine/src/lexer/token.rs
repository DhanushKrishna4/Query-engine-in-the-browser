//! Tokens and the keyword table.
//!
//! The keyword set is complete for the SQL subset in the project spec, not just
//! for the clauses the parser currently accepts. Lexing a word as a keyword the
//! parser does not yet handle produces "unexpected keyword `JOIN`" instead of
//! silently treating `JOIN` as a table alias, which is a far better error.

use crate::error::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    All, And, As, Asc, Between, By, Case, Cast, Cross, Current, Desc,
    Distinct, Else, End, Escape, Except, Exists, False, First, Following, From,
    Full, Group, Groups, Having, Ilike, In, Inner, Intersect, Is, Join, Last,
    Left, Like, Limit, Not, Null, Nulls, Offset, On, Or, Order, Outer, Over,
    Partition, Preceding, Range, Recursive, Right, Row, Rows, Select, Then,
    True, Unbounded, Union, Using, When, Where, With,
    // Type names, needed by CAST(x AS <type>).
    Bigint, Boolean, Char, Date, Decimal, Double, Float, Int, Integer, Numeric,
    Precision, Real, Smallint, Text, Timestamp, Varchar,
}

impl Keyword {
    /// SQL keywords are case-insensitive, so lookup folds to uppercase first.
    /// Not `FromStr`: a non-keyword word is an identifier, not an error.
    pub fn lookup(s: &str) -> Option<Keyword> {
        use Keyword::*;
        let upper = s.to_ascii_uppercase();
        Some(match upper.as_str() {
            "ALL" => All, "AND" => And, "AS" => As, "ASC" => Asc,
            "BETWEEN" => Between, "BY" => By, "CASE" => Case, "CAST" => Cast,
            "CROSS" => Cross, "CURRENT" => Current, "DESC" => Desc, "DISTINCT" => Distinct, "ELSE" => Else, "END" => End,
            "ESCAPE" => Escape, "EXCEPT" => Except, "EXISTS" => Exists,
            "FALSE" => False, "FIRST" => First, "FOLLOWING" => Following,
            "FROM" => From, "FULL" => Full, "GROUP" => Group, "GROUPS" => Groups,
            "HAVING" => Having, "ILIKE" => Ilike, "IN" => In, "INNER" => Inner,
            "INTERSECT" => Intersect, "IS" => Is, "JOIN" => Join, "LAST" => Last,
            "LEFT" => Left, "LIKE" => Like, "LIMIT" => Limit, "NOT" => Not,
            "NULL" => Null, "NULLS" => Nulls, "OFFSET" => Offset, "ON" => On,
            "OR" => Or, "ORDER" => Order, "OUTER" => Outer, "OVER" => Over,
            "PARTITION" => Partition, "PRECEDING" => Preceding, "RANGE" => Range,
            "RECURSIVE" => Recursive, "RIGHT" => Right, "ROW" => Row,
            "ROWS" => Rows, "SELECT" => Select, "THEN" => Then, "TRUE" => True,
            "UNBOUNDED" => Unbounded, "UNION" => Union, "USING" => Using,
            "WHEN" => When, "WHERE" => Where, "WITH" => With,
            "BIGINT" => Bigint, "BOOLEAN" | "BOOL" => Boolean, "CHAR" => Char,
            "DATE" => Date, "DECIMAL" => Decimal, "DOUBLE" => Double,
            "FLOAT" => Float, "INT" => Int, "INTEGER" => Integer,
            "NUMERIC" => Numeric, "PRECISION" => Precision, "REAL" => Real,
            "SMALLINT" => Smallint, "TEXT" => Text, "TIMESTAMP" => Timestamp,
            "VARCHAR" => Varchar,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        use Keyword::*;
        match self {
            All => "ALL", And => "AND", As => "AS", Asc => "ASC",
            Between => "BETWEEN", By => "BY", Case => "CASE", Cast => "CAST",
            Cross => "CROSS", Current => "CURRENT", Desc => "DESC", Distinct => "DISTINCT", Else => "ELSE", End => "END",
            Escape => "ESCAPE", Except => "EXCEPT", Exists => "EXISTS",
            False => "FALSE", First => "FIRST", Following => "FOLLOWING",
            From => "FROM", Full => "FULL", Group => "GROUP", Groups => "GROUPS",
            Having => "HAVING", Ilike => "ILIKE", In => "IN", Inner => "INNER",
            Intersect => "INTERSECT", Is => "IS", Join => "JOIN", Last => "LAST",
            Left => "LEFT", Like => "LIKE", Limit => "LIMIT", Not => "NOT",
            Null => "NULL", Nulls => "NULLS", Offset => "OFFSET", On => "ON",
            Or => "OR", Order => "ORDER", Outer => "OUTER", Over => "OVER",
            Partition => "PARTITION", Preceding => "PRECEDING", Range => "RANGE",
            Recursive => "RECURSIVE", Right => "RIGHT", Row => "ROW",
            Rows => "ROWS", Select => "SELECT", Then => "THEN", True => "TRUE",
            Unbounded => "UNBOUNDED", Union => "UNION", Using => "USING",
            When => "WHEN", Where => "WHERE", With => "WITH",
            Bigint => "BIGINT", Boolean => "BOOLEAN", Char => "CHAR",
            Date => "DATE", Decimal => "DECIMAL", Double => "DOUBLE",
            Float => "FLOAT", Int => "INT", Integer => "INTEGER",
            Numeric => "NUMERIC", Precision => "PRECISION", Real => "REAL",
            Smallint => "SMALLINT", Text => "TEXT", Timestamp => "TIMESTAMP",
            Varchar => "VARCHAR",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// A bare word that is not a keyword.
    Ident,
    /// A `"double quoted"` identifier. Case is preserved and it is never a keyword.
    QuotedIdent,
    Keyword(Keyword),
    /// Numeric literal. The digits are kept verbatim in `text`; deciding
    /// between Int64/Float64 is the binder's job.
    Number,
    /// `'single quoted'` string literal, already unescaped into `text`.
    String,

    LParen, RParen, Comma, Semicolon, Period,

    Plus, Minus, Star, Slash, Percent,
    /// `||` string concatenation.
    Concat,

    Eq, NotEq, Lt, LtEq, Gt, GtEq,

    Eof,
}

impl TokenKind {
    /// How this kind is named in "expected one of [...]" lists.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident | TokenKind::QuotedIdent => "identifier".into(),
            TokenKind::Keyword(k) => format!("`{}`", k.as_str()),
            TokenKind::Number => "number".into(),
            TokenKind::String => "string literal".into(),
            TokenKind::Eof => "end of input".into(),
            other => format!("`{}`", other.symbol()),
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            TokenKind::LParen => "(", TokenKind::RParen => ")",
            TokenKind::Comma => ",", TokenKind::Semicolon => ";",
            TokenKind::Period => ".", TokenKind::Plus => "+",
            TokenKind::Minus => "-", TokenKind::Star => "*",
            TokenKind::Slash => "/", TokenKind::Percent => "%",
            TokenKind::Concat => "||", TokenKind::Eq => "=",
            TokenKind::NotEq => "<>", TokenKind::Lt => "<",
            TokenKind::LtEq => "<=", TokenKind::Gt => ">",
            TokenKind::GtEq => ">=",
            _ => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// For identifiers and literals, the decoded value (quotes stripped,
    /// escapes applied). For everything else, the source text.
    pub text: String,
}

impl Token {
    pub fn is_keyword(&self, kw: Keyword) -> bool {
        self.kind == TokenKind::Keyword(kw)
    }

    /// How this token is named in an error message, e.g. ``unexpected token `FROM` ``.
    pub fn describe(&self) -> String {
        match &self.kind {
            TokenKind::Eof => "end of input".into(),
            TokenKind::String => format!("string literal `{}`", self.text),
            _ => format!("`{}`", self.text),
        }
    }
}
