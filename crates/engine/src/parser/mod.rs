//! Recursive-descent statement parser with a Pratt (precedence-climbing)
//! expression parser.
//!
//! Scope note: the statement grammar covers SELECT / FROM with joins and
//! derived tables / WHERE / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET,
//! `UNION` / `INTERSECT` / `EXCEPT`, window functions, and scalar, `EXISTS` and
//! `IN` subqueries. CTEs are lexed and then rejected with a targeted "not
//! supported yet" diagnostic rather than a generic parse error, so the message
//! tells you what is missing.
//!
//! The *expression* grammar is complete: it is what WHERE needs, and it is the
//! part the Pratt parser exists for.

pub mod ast;

pub use ast::{
    BinaryOperator, Expr, FromItem, FunctionArg, Ident, Join, JoinConstraint, JoinOperator,
    FrameBound, FrameUnits, Literal, OrderByExpr, Query, Select, SelectItem, SetExpr, SetOperator,
    Statement, TableFactor, TableRef, UnaryOperator, WindowFrame, WindowSpec,
};

use crate::error::{Diagnostic, Result, Span};
use crate::lexer::{tokenize, Keyword, Token, TokenKind};
use crate::parser::ast::Cte;
use crate::types::DataType;

/// Binding powers, lowest binds loosest. These encode standard SQL precedence:
///
/// ```text
/// OR  <  AND  <  NOT  <  (= <> < <= > >=, IS, BETWEEN, IN, LIKE)  <  ||  <  + -  <  * / %  <  unary
/// ```
const BP_OR: u8 = 1;
const BP_AND: u8 = 2;
const BP_NOT: u8 = 3;
const BP_COMPARE: u8 = 4;
const BP_CONCAT: u8 = 6;
const BP_ADD: u8 = 7;
const BP_MUL: u8 = 8;
const BP_UNARY: u8 = 9;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

/// Lex and parse a single statement.
pub fn parse(sql: &str) -> Result<Statement> {
    Parser::new(tokenize(sql)?).parse_statement()
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Parser {
        Parser { tokens, pos: 0 }
    }

    // -- token cursor -------------------------------------------------------

    fn peek(&self) -> &Token {
        // `tokenize` always appends Eof, so this never runs off the end.
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_ahead(&self, n: usize) -> &Token {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)]
    }

    fn advance(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, kind: &TokenKind) -> bool {
        &self.peek().kind == kind
    }

    fn at_keyword(&self, kw: Keyword) -> bool {
        self.peek().kind == TokenKind::Keyword(kw)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, kw: Keyword) -> bool {
        self.eat(&TokenKind::Keyword(kw))
    }

    fn expect(&mut self, kind: TokenKind) -> Result<Token> {
        if self.at(&kind) {
            Ok(self.advance())
        } else {
            Err(self.unexpected(&[kind.describe()]))
        }
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<Token> {
        self.expect(TokenKind::Keyword(kw))
    }

    /// The standard parse error: what we found, where, and what would have
    /// been acceptable instead.
    fn unexpected(&self, expected: &[String]) -> Diagnostic {
        let tok = self.peek();
        Diagnostic::parse(
            format!("unexpected token {}", tok.describe()),
            tok.span,
        )
        .with_expected(expected.to_vec())
    }

    fn unsupported(&self, feature: &str, span: Span) -> Diagnostic {
        Diagnostic::parse(format!("{feature} is not supported yet"), span).with_hint(
            "this build implements SELECT / FROM / WHERE / LIMIT / OFFSET",
        )
    }

    // -- statements ---------------------------------------------------------

    pub fn parse_statement(&mut self) -> Result<Statement> {
        let query = self.parse_query()?;

        // A statement may be terminated by `;` but nothing may follow it.
        self.eat(&TokenKind::Semicolon);
        if !self.at(&TokenKind::Eof) {
            return Err(self.unexpected(&["end of input".to_string()]));
        }
        Ok(Statement::Query(query))
    }

    fn parse_query(&mut self) -> Result<Query> {
        let start = self.peek().span.start;
        let with = self.parse_with()?;
        let body = self.parse_set_expr()?;

        let mut order_by = Vec::new();
        if self.eat_keyword(Keyword::Order) {
            self.expect_keyword(Keyword::By)?;
            order_by.push(self.parse_order_by_expr()?);
            while self.eat(&TokenKind::Comma) {
                order_by.push(self.parse_order_by_expr()?);
            }
        }

        let limit = if self.eat_keyword(Keyword::Limit) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let offset = if self.eat_keyword(Keyword::Offset) {
            let e = self.parse_expr(0)?;
            // `OFFSET n ROW[S]` is the standard spelling; accept and ignore the noise word.
            let _ = self.eat_keyword(Keyword::Rows) || self.eat_keyword(Keyword::Row);
            Some(e)
        } else {
            None
        };

        let end = self.prev_end(start);
        Ok(Query {
            with,
            body,
            order_by,
            limit,
            offset,
            span: Span::new(start, end),
        })
    }

    /// `WITH name [(cols)] AS ( query ) [, ...]`, or nothing.
    ///
    /// `RECURSIVE` is rejected by name rather than ignored. A recursive CTE is
    /// a fixpoint computation, not a named subquery, and binding one as if it
    /// were the latter would produce an answer that looks reasonable and is
    /// wrong -- the worst kind.
    fn parse_with(&mut self) -> Result<Vec<Cte>> {
        if !self.eat_keyword(Keyword::With) {
            return Ok(Vec::new());
        }
        if self.at_keyword(Keyword::Recursive) {
            return Err(self.unsupported("recursive common table expressions", self.peek().span));
        }

        let mut out = Vec::new();
        loop {
            let start = self.peek().span.start;
            let name = self.parse_ident()?;

            let mut columns = Vec::new();
            if self.at(&TokenKind::LParen) && self.peek_is_column_list() {
                self.expect(TokenKind::LParen)?;
                loop {
                    columns.push(self.parse_ident()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RParen)?;
            }

            self.expect_keyword(Keyword::As)?;
            self.expect(TokenKind::LParen)?;
            let query = self.parse_query()?;
            self.expect(TokenKind::RParen)?;

            let end = self.prev_end(start);
            out.push(Cte {
                name,
                columns,
                query,
                span: Span::new(start, end),
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(out)
    }

    /// Distinguish `WITH t(a, b) AS ...` from `WITH t AS (SELECT ...)`.
    ///
    /// Both may show a parenthesis after the name. Only a column list has a
    /// bare identifier immediately inside it followed by a comma or the close.
    fn peek_is_column_list(&self) -> bool {
        let is_ident = matches!(
            self.peek_ahead(1).kind,
            TokenKind::Ident | TokenKind::QuotedIdent
        );
        is_ident
            && matches!(
                self.peek_ahead(2).kind,
                TokenKind::Comma | TokenKind::RParen
            )
    }

    /// SELECTs combined by set operators, left-associative, all at one
    /// precedence -- which is what the standard says and what SQLite does.
    fn parse_set_expr(&mut self) -> Result<SetExpr> {
        let start = self.peek().span;
        let mut left = SetExpr::Select(Box::new(self.parse_select()?));

        loop {
            let op = if self.eat_keyword(Keyword::Union) {
                SetOperator::Union
            } else if self.eat_keyword(Keyword::Intersect) {
                SetOperator::Intersect
            } else if self.eat_keyword(Keyword::Except) {
                SetOperator::Except
            } else {
                return Ok(left);
            };
            // `ALL` keeps duplicates; `DISTINCT` is the default and may be
            // written out.
            let all = if self.eat_keyword(Keyword::All) {
                true
            } else {
                self.eat_keyword(Keyword::Distinct);
                false
            };
            let right = SetExpr::Select(Box::new(self.parse_select()?));
            left = SetExpr::SetOp {
                op,
                all,
                left: Box::new(left),
                right: Box::new(right),
                span: start.merge(self.prev_span()),
            };
        }
    }

    fn parse_order_by_expr(&mut self) -> Result<OrderByExpr> {
        let expr = self.parse_expr(0)?;
        // `None` means unspecified, which is not the same as ASC: the default
        // NULL ordering depends on the direction, and an unspecified direction
        // has to be distinguishable to apply it.
        let asc = if self.eat_keyword(Keyword::Asc) {
            Some(true)
        } else if self.eat_keyword(Keyword::Desc) {
            Some(false)
        } else {
            None
        };
        let nulls_first = if self.eat_keyword(Keyword::Nulls) {
            if self.eat_keyword(Keyword::First) {
                Some(true)
            } else {
                self.expect_keyword(Keyword::Last)?;
                Some(false)
            }
        } else {
            None
        };
        Ok(OrderByExpr {
            expr,
            asc,
            nulls_first,
        })
    }

    fn prev_end(&self, fallback: usize) -> usize {
        if self.pos == 0 {
            fallback
        } else {
            self.tokens[self.pos - 1].span.end
        }
    }

    fn parse_select(&mut self) -> Result<Select> {
        let start = self.peek().span.start;
        self.expect_keyword(Keyword::Select)?;

        // DISTINCT / ALL. `ALL` is the default and carries no meaning.
        let distinct = if self.eat_keyword(Keyword::Distinct) {
            true
        } else {
            self.eat_keyword(Keyword::All);
            false
        };

        let mut projection = vec![self.parse_select_item()?];
        while self.eat(&TokenKind::Comma) {
            projection.push(self.parse_select_item()?);
        }

        // A FROM-less SELECT (`SELECT 1 + 1`) is legal and binds against a
        // single-row, zero-column relation.
        let mut from = Vec::new();
        if self.eat_keyword(Keyword::From) {
            from.push(self.parse_from_item()?);
            while self.eat(&TokenKind::Comma) {
                from.push(self.parse_from_item()?);
            }
        }

        let selection = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_keyword(Keyword::Group) {
            self.expect_keyword(Keyword::By)?;
            group_by.push(self.parse_expr(0)?);
            while self.eat(&TokenKind::Comma) {
                group_by.push(self.parse_expr(0)?);
            }
        }

        let having = if self.eat_keyword(Keyword::Having) {
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let end = self.prev_end(start);
        Ok(Select {
            distinct,
            projection,
            from,
            selection,
            group_by,
            having,
            span: Span::new(start, end),
        })
    }

    /// A relation and everything joined onto it. Joins associate to the left.
    fn parse_from_item(&mut self) -> Result<ast::FromItem> {
        let relation = self.parse_table_ref()?;
        let mut joins = Vec::new();
        while let Some(join) = self.parse_join()? {
            joins.push(join);
        }
        Ok(ast::FromItem { relation, joins })
    }

    fn parse_join(&mut self) -> Result<Option<ast::Join>> {
        let start = self.peek().span;

        // `INNER`, `LEFT`, `RIGHT` and `FULL` are only join keywords when a
        // `JOIN` follows, and `OUTER` is noise the standard allows after the
        // outer-join kinds.
        let operator = if self.eat_keyword(Keyword::Cross) {
            self.expect_keyword(Keyword::Join)?;
            ast::JoinOperator::Cross
        } else if self.eat_keyword(Keyword::Inner) {
            self.expect_keyword(Keyword::Join)?;
            ast::JoinOperator::Inner
        } else if self.eat_keyword(Keyword::Left) {
            self.eat_keyword(Keyword::Outer);
            self.expect_keyword(Keyword::Join)?;
            ast::JoinOperator::Left
        } else if self.eat_keyword(Keyword::Right) {
            self.eat_keyword(Keyword::Outer);
            self.expect_keyword(Keyword::Join)?;
            ast::JoinOperator::Right
        } else if self.eat_keyword(Keyword::Full) {
            self.eat_keyword(Keyword::Outer);
            self.expect_keyword(Keyword::Join)?;
            ast::JoinOperator::Full
        } else if self.eat_keyword(Keyword::Join) {
            ast::JoinOperator::Inner
        } else {
            return Ok(None);
        };

        let relation = self.parse_table_ref()?;

        let constraint = if self.eat_keyword(Keyword::On) {
            ast::JoinConstraint::On(self.parse_expr(0)?)
        } else if self.eat_keyword(Keyword::Using) {
            self.expect(TokenKind::LParen)?;
            let mut cols = vec![self.parse_ident()?];
            while self.eat(&TokenKind::Comma) {
                cols.push(self.parse_ident()?);
            }
            self.expect(TokenKind::RParen)?;
            ast::JoinConstraint::Using(cols)
        } else {
            ast::JoinConstraint::None
        };

        // A cross join must not carry a condition, and every other kind must.
        match (&operator, &constraint) {
            (ast::JoinOperator::Cross, ast::JoinConstraint::None) => {}
            (ast::JoinOperator::Cross, _) => {
                return Err(Diagnostic::parse(
                    "CROSS JOIN does not take a condition",
                    start.merge(self.prev_span()),
                )
                .with_hint("use an inner join with ON, or move the condition to WHERE"))
            }
            (_, ast::JoinConstraint::None) => {
                return Err(self.unexpected(&["`ON`".into(), "`USING`".into()]))
            }
            _ => {}
        }

        Ok(Some(ast::Join {
            operator,
            relation,
            constraint,
            span: start.merge(self.prev_span()),
        }))
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            Span::EMPTY
        } else {
            self.tokens[self.pos - 1].span
        }
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        // Bare `*`.
        if self.at(&TokenKind::Star) {
            let span = self.advance().span;
            return Ok(SelectItem::Wildcard { span });
        }
        // `t.*` -- checked with two tokens of lookahead so that `t.a` still
        // falls through to the ordinary expression path.
        if matches!(self.peek().kind, TokenKind::Ident | TokenKind::QuotedIdent)
            && self.peek_ahead(1).kind == TokenKind::Period
            && self.peek_ahead(2).kind == TokenKind::Star
        {
            let qualifier = self.parse_ident()?;
            self.advance(); // .
            let star = self.advance(); // *
            let span = qualifier.span.merge(star.span);
            return Ok(SelectItem::QualifiedWildcard { qualifier, span });
        }

        let expr = self.parse_expr(0)?;
        let alias = self.parse_optional_alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    /// `AS name`, or a bare `name`. Keywords never become implicit aliases,
    /// which is what stops `SELECT a FROM t` from reading `FROM` as an alias.
    fn parse_optional_alias(&mut self) -> Result<Option<Ident>> {
        if self.eat_keyword(Keyword::As) {
            return Ok(Some(self.parse_ident()?));
        }
        if matches!(self.peek().kind, TokenKind::Ident | TokenKind::QuotedIdent) {
            return Ok(Some(self.parse_ident()?));
        }
        Ok(None)
    }

    fn parse_table_ref(&mut self) -> Result<TableRef> {
        let start = self.peek().span;
        if self.eat(&TokenKind::LParen) {
            let query = self.parse_query()?;
            self.expect(TokenKind::RParen)?;
            // The alias is not optional. A derived table has no name of its
            // own, so without one its columns could not be qualified and an
            // ambiguity could not be resolved.
            let alias = match self.parse_optional_alias()? {
                Some(a) => a,
                None => {
                    return Err(Diagnostic::parse(
                        "a derived table needs an alias",
                        start.merge(self.prev_span()),
                    )
                    .with_hint("write `(SELECT ...) AS name`"))
                }
            };
            let span = start.merge(alias.span);
            return Ok(TableRef {
                factor: ast::TableFactor::Derived(Box::new(query)),
                alias: Some(alias),
                span,
            });
        }
        let name = self.parse_ident()?;
        let alias = self.parse_optional_alias()?;
        let span = match &alias {
            Some(a) => name.span.merge(a.span),
            None => name.span,
        };
        Ok(TableRef {
            factor: ast::TableFactor::Table(name),
            alias,
            span,
        })
    }

    fn parse_ident(&mut self) -> Result<Ident> {
        match self.peek().kind {
            TokenKind::Ident => {
                let t = self.advance();
                Ok(Ident { value: t.text, quoted: false, span: t.span })
            }
            TokenKind::QuotedIdent => {
                let t = self.advance();
                Ok(Ident { value: t.text, quoted: true, span: t.span })
            }
            _ => Err(self.unexpected(&["identifier".to_string()])),
        }
    }

    // -- expressions (Pratt) ------------------------------------------------

    /// Parse an expression, stopping when the next operator binds more loosely
    /// than `min_bp`. Call with 0 for a complete expression.
    pub fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.parse_prefix()?;

        loop {
            // Postfix-style operators (IS NULL, BETWEEN, IN, LIKE) all sit at
            // comparison precedence. They are checked before the infix table
            // because `NOT` here means `NOT IN` / `NOT LIKE` / `NOT BETWEEN`
            // rather than the prefix negation operator.
            if BP_COMPARE >= min_bp && self.at_postfix_operator() {
                lhs = self.parse_postfix(lhs)?;
                continue;
            }

            let Some((op, bp)) = self.peek_infix() else { break };
            if bp < min_bp {
                break;
            }
            let op_span = self.advance().span;
            // `bp + 1` on the right-hand side makes every binary operator
            // left-associative: `a - b - c` parses as `(a - b) - c`.
            let rhs = self.parse_expr(bp + 1)?;
            lhs = Expr::BinaryOp {
                left: Box::new(lhs),
                op,
                right: Box::new(rhs),
                op_span,
            };
        }
        Ok(lhs)
    }

    fn peek_infix(&self) -> Option<(BinaryOperator, u8)> {
        use BinaryOperator as B;
        Some(match &self.peek().kind {
            TokenKind::Keyword(Keyword::Or) => (B::Or, BP_OR),
            TokenKind::Keyword(Keyword::And) => (B::And, BP_AND),
            TokenKind::Eq => (B::Eq, BP_COMPARE),
            TokenKind::NotEq => (B::NotEq, BP_COMPARE),
            TokenKind::Lt => (B::Lt, BP_COMPARE),
            TokenKind::LtEq => (B::LtEq, BP_COMPARE),
            TokenKind::Gt => (B::Gt, BP_COMPARE),
            TokenKind::GtEq => (B::GtEq, BP_COMPARE),
            TokenKind::Concat => (B::StringConcat, BP_CONCAT),
            TokenKind::Plus => (B::Plus, BP_ADD),
            TokenKind::Minus => (B::Minus, BP_ADD),
            TokenKind::Star => (B::Multiply, BP_MUL),
            TokenKind::Slash => (B::Divide, BP_MUL),
            TokenKind::Percent => (B::Modulo, BP_MUL),
            _ => return None,
        })
    }

    fn at_postfix_operator(&self) -> bool {
        let direct = matches!(
            self.peek().kind,
            TokenKind::Keyword(Keyword::Is)
                | TokenKind::Keyword(Keyword::Between)
                | TokenKind::Keyword(Keyword::In)
                | TokenKind::Keyword(Keyword::Like)
                | TokenKind::Keyword(Keyword::Ilike)
        );
        let negated = self.at_keyword(Keyword::Not)
            && matches!(
                self.peek_ahead(1).kind,
                TokenKind::Keyword(Keyword::Between)
                    | TokenKind::Keyword(Keyword::In)
                    | TokenKind::Keyword(Keyword::Like)
                    | TokenKind::Keyword(Keyword::Ilike)
            );
        direct || negated
    }

    fn parse_postfix(&mut self, lhs: Expr) -> Result<Expr> {
        let start = lhs.span();
        let negated = self.eat_keyword(Keyword::Not);

        if self.eat_keyword(Keyword::Is) {
            // Only IS [NOT] NULL is supported; IS TRUE / IS DISTINCT FROM are
            // separate features with separate semantics.
            let not = self.eat_keyword(Keyword::Not);
            let null_tok = self.expect_keyword(Keyword::Null).map_err(|d| {
                Diagnostic::parse("expected `NULL` after `IS`", d.span.unwrap_or_default())
                    .with_expected(["`NULL`"])
            })?;
            return Ok(Expr::IsNull {
                expr: Box::new(lhs),
                negated: not,
                span: start.merge(null_tok.span),
            });
        }

        if self.eat_keyword(Keyword::Between) {
            // The bounds are parsed above AND precedence so that the `AND` in
            // `BETWEEN a AND b` belongs to BETWEEN and not to a logical AND.
            let low = self.parse_expr(BP_AND + 1)?;
            self.expect_keyword(Keyword::And)?;
            let high = self.parse_expr(BP_AND + 1)?;
            let span = start.merge(high.span());
            return Ok(Expr::Between {
                expr: Box::new(lhs),
                negated,
                low: Box::new(low),
                high: Box::new(high),
                span,
            });
        }

        if self.eat_keyword(Keyword::In) {
            self.expect(TokenKind::LParen)?;
            if self.at_keyword(Keyword::Select) || self.at_keyword(Keyword::With) {
                let query = self.parse_query()?;
                let close = self.expect(TokenKind::RParen)?;
                return Ok(Expr::InSubquery {
                    expr: Box::new(lhs),
                    query: Box::new(query),
                    negated,
                    span: start.merge(close.span),
                });
            }
            let mut list = vec![self.parse_expr(0)?];
            while self.eat(&TokenKind::Comma) {
                list.push(self.parse_expr(0)?);
            }
            let close = self.expect(TokenKind::RParen)?;
            return Ok(Expr::InList {
                expr: Box::new(lhs),
                list,
                negated,
                span: start.merge(close.span),
            });
        }

        let case_insensitive = if self.eat_keyword(Keyword::Like) {
            false
        } else if self.eat_keyword(Keyword::Ilike) {
            true
        } else {
            return Err(self.unexpected(&["`IN`".into(), "`LIKE`".into(), "`BETWEEN`".into()]));
        };
        // The pattern binds tighter than AND/OR but looser than `||`, so
        // `x LIKE 'a' || 'b' AND y` groups as `(x LIKE ('a'||'b')) AND y`.
        let pattern = self.parse_expr(BP_COMPARE + 1)?;
        let escape = if self.eat_keyword(Keyword::Escape) {
            Some(Box::new(self.parse_expr(BP_COMPARE + 1)?))
        } else {
            None
        };
        let end = escape
            .as_ref()
            .map(|e| e.span())
            .unwrap_or_else(|| pattern.span());
        Ok(Expr::Like {
            expr: Box::new(lhs),
            pattern: Box::new(pattern),
            negated,
            case_insensitive,
            escape,
            span: start.merge(end),
        })
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        let tok = self.peek().clone();
        match &tok.kind {
            TokenKind::Keyword(Keyword::Not) => {
                self.advance();
                // `NOT EXISTS` is one operator, not a negation applied to one.
                // The two mean the same thing, but keeping them together lets
                // decorrelation see an anti-join directly.
                if self.at_keyword(Keyword::Exists) {
                    return self.parse_exists(true);
                }
                // NOT binds looser than comparison: `NOT a = b` is `NOT (a = b)`.
                let expr = self.parse_expr(BP_NOT)?;
                let span = tok.span.merge(expr.span());
                Ok(Expr::UnaryOp { op: UnaryOperator::Not, expr: Box::new(expr), span })
            }
            TokenKind::Minus | TokenKind::Plus => {
                self.advance();
                let op = if tok.kind == TokenKind::Minus {
                    UnaryOperator::Minus
                } else {
                    UnaryOperator::Plus
                };
                let expr = self.parse_expr(BP_UNARY)?;
                let span = tok.span.merge(expr.span());
                Ok(Expr::UnaryOp { op, expr: Box::new(expr), span })
            }
            TokenKind::Number => {
                self.advance();
                Ok(Expr::Literal { value: Literal::Number(tok.text), span: tok.span })
            }
            TokenKind::String => {
                self.advance();
                Ok(Expr::Literal { value: Literal::String(tok.text), span: tok.span })
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.advance();
                Ok(Expr::Literal { value: Literal::Null, span: tok.span })
            }
            TokenKind::Keyword(Keyword::True) => {
                self.advance();
                Ok(Expr::Literal { value: Literal::Boolean(true), span: tok.span })
            }
            TokenKind::Keyword(Keyword::False) => {
                self.advance();
                Ok(Expr::Literal { value: Literal::Boolean(false), span: tok.span })
            }
            TokenKind::LParen => {
                self.advance();
                if self.at_keyword(Keyword::Select) || self.at_keyword(Keyword::With) {
                    let query = self.parse_query()?;
                    let close = self.expect(TokenKind::RParen)?;
                    return Ok(Expr::ScalarSubquery {
                        query: Box::new(query),
                        span: tok.span.merge(close.span),
                    });
                }
                let inner = self.parse_expr(0)?;
                let close = self.expect(TokenKind::RParen)?;
                Ok(Expr::Nested {
                    expr: Box::new(inner),
                    span: tok.span.merge(close.span),
                })
            }
            TokenKind::Keyword(Keyword::Case) => self.parse_case(),
            TokenKind::Keyword(Keyword::Cast) => self.parse_cast(),
            TokenKind::Keyword(Keyword::Exists) => self.parse_exists(false),
            TokenKind::Ident | TokenKind::QuotedIdent => self.parse_ident_expr(),
            _ => Err(self.unexpected(&[
                "identifier".into(),
                "literal".into(),
                "`(`".into(),
                "`NOT`".into(),
                "`CASE`".into(),
                "`CAST`".into(),
            ])),
        }
    }

    fn parse_ident_expr(&mut self) -> Result<Expr> {
        let first = self.parse_ident()?;

        if self.at(&TokenKind::LParen) {
            return self.parse_function(first);
        }

        if self.at(&TokenKind::Period) {
            let mut parts = vec![first];
            while self.eat(&TokenKind::Period) {
                if self.at(&TokenKind::Star) {
                    let span = self.peek().span;
                    return Err(Diagnostic::parse(
                        "`*` is only allowed directly in a SELECT list",
                        span,
                    ));
                }
                parts.push(self.parse_ident()?);
            }
            return Ok(Expr::CompoundIdentifier(parts));
        }

        Ok(Expr::Identifier(first))
    }

    fn parse_function(&mut self, name: Ident) -> Result<Expr> {
        self.expect(TokenKind::LParen)?;
        let distinct = self.eat_keyword(Keyword::Distinct);

        let mut args = Vec::new();
        if !self.at(&TokenKind::RParen) {
            loop {
                if self.at(&TokenKind::Star) {
                    // Only `COUNT(*)` shaped calls; the binder decides whether
                    // the function actually accepts a wildcard.
                    args.push(FunctionArg::Wildcard(self.advance().span));
                } else {
                    args.push(FunctionArg::Expr(self.parse_expr(0)?));
                }
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let close = self.expect(TokenKind::RParen)?;

        let over = if self.at_keyword(Keyword::Over) {
            Some(self.parse_window_spec()?)
        } else {
            None
        };

        let end = over.as_ref().map_or(close.span, |o| o.span);
        Ok(Expr::Function {
            span: name.span.merge(end),
            name,
            args,
            distinct,
            over,
        })
    }

    fn parse_window_spec(&mut self) -> Result<WindowSpec> {
        let start = self.expect_keyword(Keyword::Over)?.span;
        self.expect(TokenKind::LParen)?;

        let mut partition_by = Vec::new();
        if self.eat_keyword(Keyword::Partition) {
            self.expect_keyword(Keyword::By)?;
            partition_by.push(self.parse_expr(0)?);
            while self.eat(&TokenKind::Comma) {
                partition_by.push(self.parse_expr(0)?);
            }
        }

        let mut order_by = Vec::new();
        if self.eat_keyword(Keyword::Order) {
            self.expect_keyword(Keyword::By)?;
            order_by.push(self.parse_order_by_expr()?);
            while self.eat(&TokenKind::Comma) {
                order_by.push(self.parse_order_by_expr()?);
            }
        }

        let frame = self.parse_window_frame()?;
        let close = self.expect(TokenKind::RParen)?;
        Ok(WindowSpec {
            partition_by,
            order_by,
            frame,
            span: start.merge(close.span),
        })
    }

    fn parse_window_frame(&mut self) -> Result<Option<WindowFrame>> {
        let units = if self.eat_keyword(Keyword::Rows) {
            FrameUnits::Rows
        } else if self.eat_keyword(Keyword::Range) {
            FrameUnits::Range
        } else if self.at_keyword(Keyword::Groups) {
            return Err(self.unsupported("GROUPS window frames", self.peek().span));
        } else {
            return Ok(None);
        };

        // `ROWS <bound>` is shorthand for `ROWS BETWEEN <bound> AND CURRENT ROW`.
        if !self.eat_keyword(Keyword::Between) {
            let start = self.parse_frame_bound()?;
            return Ok(Some(WindowFrame {
                units,
                start,
                end: FrameBound::CurrentRow,
            }));
        }
        let start = self.parse_frame_bound()?;
        self.expect_keyword(Keyword::And)?;
        let end = self.parse_frame_bound()?;
        Ok(Some(WindowFrame { units, start, end }))
    }

    fn parse_frame_bound(&mut self) -> Result<FrameBound> {
        if self.eat_keyword(Keyword::Unbounded) {
            return if self.eat_keyword(Keyword::Preceding) {
                Ok(FrameBound::UnboundedPreceding)
            } else {
                self.expect_keyword(Keyword::Following)?;
                Ok(FrameBound::UnboundedFollowing)
            };
        }
        if self.eat_keyword(Keyword::Current) {
            self.expect_keyword(Keyword::Row)?;
            return Ok(FrameBound::CurrentRow);
        }
        let offset = self.parse_expr(0)?;
        if self.eat_keyword(Keyword::Preceding) {
            Ok(FrameBound::Preceding(Box::new(offset)))
        } else {
            self.expect_keyword(Keyword::Following)?;
            Ok(FrameBound::Following(Box::new(offset)))
        }
    }

    fn parse_exists(&mut self, negated: bool) -> Result<Expr> {
        let start = self.expect_keyword(Keyword::Exists)?.span;
        self.expect(TokenKind::LParen)?;
        let query = self.parse_query()?;
        let close = self.expect(TokenKind::RParen)?;
        Ok(Expr::Exists {
            query: Box::new(query),
            negated,
            span: start.merge(close.span),
        })
    }

    fn parse_case(&mut self) -> Result<Expr> {
        let start = self.expect_keyword(Keyword::Case)?.span;

        // `CASE x WHEN ...` (simple) versus `CASE WHEN ...` (searched).
        let operand = if self.at_keyword(Keyword::When) {
            None
        } else {
            Some(Box::new(self.parse_expr(0)?))
        };

        let mut when_then = Vec::new();
        while self.eat_keyword(Keyword::When) {
            let when = self.parse_expr(0)?;
            self.expect_keyword(Keyword::Then)?;
            let then = self.parse_expr(0)?;
            when_then.push((when, then));
        }
        if when_then.is_empty() {
            return Err(self.unexpected(&["`WHEN`".to_string()]));
        }

        let else_result = if self.eat_keyword(Keyword::Else) {
            Some(Box::new(self.parse_expr(0)?))
        } else {
            None
        };
        let end = self.expect_keyword(Keyword::End)?.span;

        Ok(Expr::Case {
            operand,
            when_then,
            else_result,
            span: start.merge(end),
        })
    }

    fn parse_cast(&mut self) -> Result<Expr> {
        let start = self.expect_keyword(Keyword::Cast)?.span;
        self.expect(TokenKind::LParen)?;
        let expr = self.parse_expr(0)?;
        self.expect_keyword(Keyword::As)?;
        let data_type = self.parse_data_type()?;
        let close = self.expect(TokenKind::RParen)?;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            data_type,
            span: start.merge(close.span),
        })
    }

    /// SQL type names, mapped onto the engine's logical types. Width variants
    /// that the storage layer does not distinguish (SMALLINT, VARCHAR(n))
    /// collapse onto the nearest type it does.
    fn parse_data_type(&mut self) -> Result<DataType> {
        let tok = self.peek().clone();
        let TokenKind::Keyword(kw) = tok.kind else {
            return Err(self.unexpected(&["a type name".to_string()]));
        };
        self.advance();
        let dt = match kw {
            Keyword::Smallint | Keyword::Int | Keyword::Integer => DataType::Int32,
            Keyword::Bigint => DataType::Int64,
            Keyword::Real | Keyword::Float => DataType::Float64,
            Keyword::Double => {
                self.eat_keyword(Keyword::Precision);
                DataType::Float64
            }
            Keyword::Boolean => DataType::Boolean,
            Keyword::Date => DataType::Date32,
            Keyword::Timestamp => DataType::Timestamp,
            Keyword::Text => DataType::Utf8,
            Keyword::Varchar | Keyword::Char => {
                self.eat_optional_type_args()?;
                DataType::Utf8
            }
            Keyword::Decimal | Keyword::Numeric => {
                let (precision, scale) = match self.eat_optional_type_args()? {
                    Some((p, Some(s))) => (p, s),
                    Some((p, None)) => (p, 0),
                    None => (38, 10),
                };
                if precision == 0 || precision > 38 {
                    return Err(Diagnostic::parse(
                        format!("decimal precision must be between 1 and 38, got {precision}"),
                        tok.span,
                    ));
                }
                DataType::Decimal128 {
                    precision: precision as u8,
                    scale: scale as i8,
                }
            }
            _ => return Err(self.unexpected(&["a type name".to_string()])),
        };
        Ok(dt)
    }

    /// The optional `(p)` or `(p, s)` after a type name.
    fn eat_optional_type_args(&mut self) -> Result<Option<(i64, Option<i64>)>> {
        if !self.eat(&TokenKind::LParen) {
            return Ok(None);
        }
        let first = self.expect_number()?;
        let second = if self.eat(&TokenKind::Comma) {
            Some(self.expect_number()?)
        } else {
            None
        };
        self.expect(TokenKind::RParen)?;
        Ok(Some((first, second)))
    }

    fn expect_number(&mut self) -> Result<i64> {
        let tok = self.expect(TokenKind::Number)?;
        tok.text.parse::<i64>().map_err(|_| {
            Diagnostic::parse(format!("expected an integer, found `{}`", tok.text), tok.span)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot helper: parse, pretty-print, and compare against the expected
    /// tree. Written by hand rather than with `insta` so the engine crate stays
    /// dependency-free; the shape of the assertion is the same.
    fn snapshot(sql: &str, expected: &str) {
        let stmt = parse(sql).unwrap_or_else(|e| panic!("{}", e.render(sql)));
        let got = ast::pretty(&stmt);
        assert_eq!(got.trim_end(), expected.trim_end(), "\n--- got ---\n{got}");
    }

    fn expr_snapshot(sql: &str, expected: &str) {
        snapshot(
            &format!("SELECT {sql}"),
            &format!("Query\n  Select\n    Projection\n      Item\n{}", indent(expected, 4)),
        );
    }

    fn indent(text: &str, levels: usize) -> String {
        let pad = "  ".repeat(levels);
        text.lines()
            .map(|l| format!("{pad}{l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn parse_err(sql: &str) -> Diagnostic {
        parse(sql).unwrap_err()
    }

    #[test]
    fn simple_filtered_scan() {
        snapshot(
            "SELECT a FROM t WHERE b > 5",
            "\
Query
  Select
    Projection
      Item
        Column a
    From
      Table t
    Where
      BinaryOp >
        Column b
        Literal 5",
        );
    }

    #[test]
    fn wildcards_and_aliases() {
        snapshot(
            "SELECT *, t.*, a AS x, b y FROM tbl AS t",
            "\
Query
  Select
    Projection
      *
      t.*
      Alias x
        Column a
      Alias y
        Column b
    From
      Table tbl AS t",
        );
    }

    #[test]
    fn arithmetic_precedence_and_left_associativity() {
        // * binds tighter than +, and - is left-associative.
        expr_snapshot(
            "1 + 2 * 3",
            "\
BinaryOp +
  Literal 1
  BinaryOp *
    Literal 2
    Literal 3",
        );
        expr_snapshot(
            "1 - 2 - 3",
            "\
BinaryOp -
  BinaryOp -
    Literal 1
    Literal 2
  Literal 3",
        );
    }

    #[test]
    fn logical_precedence() {
        // AND binds tighter than OR.
        expr_snapshot(
            "a OR b AND c",
            "\
BinaryOp OR
  Column a
  BinaryOp AND
    Column b
    Column c",
        );
        // NOT binds looser than comparison but tighter than AND.
        expr_snapshot(
            "NOT a = 1 AND b",
            "\
BinaryOp AND
  UnaryOp NOT
    BinaryOp =
      Column a
      Literal 1
  Column b",
        );
    }

    #[test]
    fn unary_minus_binds_tighter_than_arithmetic() {
        expr_snapshot(
            "-a + b",
            "\
BinaryOp +
  UnaryOp -
    Column a
  Column b",
        );
        // `a-1` is subtraction, not `a` followed by the literal -1.
        expr_snapshot(
            "a-1",
            "\
BinaryOp -
  Column a
  Literal 1",
        );
    }

    #[test]
    fn between_does_not_swallow_a_following_and() {
        expr_snapshot(
            "a BETWEEN 1 AND 2 AND c",
            "\
BinaryOp AND
  Between
    Column a
    low
      Literal 1
    high
      Literal 2
  Column c",
        );
    }

    #[test]
    fn negated_postfix_operators() {
        expr_snapshot(
            "a NOT IN (1, 2)",
            "\
NotInList
  Column a
  list
    Literal 1
    Literal 2",
        );
        expr_snapshot(
            "a NOT LIKE 'x%'",
            "\
NotLike
  Column a
  pattern
    Literal 'x%'",
        );
        expr_snapshot(
            "a IS NOT NULL",
            "\
IsNotNull
  Column a",
        );
    }

    #[test]
    fn case_cast_and_functions() {
        expr_snapshot(
            "CASE WHEN a > 1 THEN 'big' ELSE 'small' END",
            "\
Case
  when
    BinaryOp >
      Column a
      Literal 1
  then
    Literal 'big'
  else
    Literal 'small'",
        );
        expr_snapshot(
            "CAST(a AS BIGINT)",
            "\
Cast to INT64
  Column a",
        );
        expr_snapshot(
            "COUNT(*)",
            "\
Function COUNT
  *",
        );
        expr_snapshot(
            "COUNT(DISTINCT a)",
            "\
Function COUNT distinct
  Column a",
        );
    }

    #[test]
    fn qualified_columns() {
        expr_snapshot("t.a", "Column t.a");
    }

    #[test]
    fn quoted_identifiers_survive_parsing() {
        let stmt = parse(r#"SELECT "Select" FROM t"#).unwrap();
        let Statement::Query(q) = stmt;
        let select = q.as_select().expect("a plain SELECT");
        let SelectItem::Expr { expr: Expr::Identifier(i), .. } = &select.projection[0] else {
            panic!("expected identifier");
        };
        assert!(i.quoted);
        assert_eq!(i.value, "Select");
        assert_eq!(i.normalized(), "Select");
    }

    #[test]
    fn from_less_select_is_allowed() {
        snapshot(
            "SELECT 1 + 1",
            "\
Query
  Select
    Projection
      Item
        BinaryOp +
          Literal 1
          Literal 1",
        );
    }

    #[test]
    fn limit_and_offset() {
        snapshot(
            "SELECT a FROM t LIMIT 10 OFFSET 5",
            "\
Query
  Select
    Projection
      Item
        Column a
    From
      Table t
  Limit
    Literal 10
  Offset
    Literal 5",
        );
    }

    #[test]
    fn parse_errors_name_the_token_and_the_alternatives() {
        let e = parse_err("SELECT FROM t");
        assert_eq!(e.message, "unexpected token `FROM`");
        assert!(e.expected.contains(&"identifier".to_string()));
        let rendered = e.render("SELECT FROM t");
        assert!(rendered.contains("line 1, column 8"), "{rendered}");
        assert!(rendered.contains("expected one of ["), "{rendered}");

        let e = parse_err("SELECT a FROM");
        assert!(e.expected.contains(&"identifier".to_string()));

        let e = parse_err("SELECT a FROM t WHERE b >");
        assert_eq!(e.message, "unexpected token end of input");

        let e = parse_err("SELECT a FROM t WHERE b IS 5");
        assert_eq!(e.message, "expected `NULL` after `IS`");
    }

    #[test]
    fn unimplemented_clauses_say_so_by_name() {
        for (sql, needle) in [
            // CTEs are supported; recursion is not, and is refused by name
            // rather than bound as if it were an ordinary named subquery.
            ("WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r", "recursive common table expressions"),
            ("SELECT SUM(a) OVER (GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
             "GROUPS window frames"),
        ] {
            let e = parse_err(sql);
            assert!(
                e.message.contains(needle),
                "for `{sql}` expected a message mentioning `{needle}`, got `{}`",
                e.message
            );
            assert!(e.span.is_some(), "`{sql}` produced no span");
        }
    }

    #[test]
    fn joins_and_grouping_parse() {
        snapshot(
            "SELECT a.x, b.y FROM a LEFT OUTER JOIN b ON a.id = b.id",
            "\
Query
  Select
    Projection
      Item
        Column a.x
      Item
        Column b.y
    From
      Table a
      LEFT join
        Table b
        on
          BinaryOp =
            Column a.id
            Column b.id",
        );

        // A comma between FROM items is a cross join.
        snapshot(
            "SELECT * FROM a, b",
            "\
Query
  Select
    Projection
      *
    From
      Table a
      Table b",
        );

        snapshot(
            "SELECT city, COUNT(*) FROM t GROUP BY city HAVING COUNT(*) > 1",
            "\
Query
  Select
    Projection
      Item
        Column city
      Item
        Function COUNT
          *
    From
      Table t
    GroupBy
      Column city
    Having
      BinaryOp >
        Function COUNT
          *
        Literal 1",
        );
    }

    #[test]
    fn join_conditions_are_required_and_cross_joins_forbid_them() {
        let e = parse_err("SELECT * FROM a JOIN b");
        assert!(e.expected.contains(&"`ON`".to_string()), "{:?}", e.expected);

        let e = parse_err("SELECT * FROM a CROSS JOIN b ON a.x = b.x");
        assert!(
            e.message.contains("CROSS JOIN does not take a condition"),
            "{}",
            e.message
        );
    }

    #[test]
    fn ordering_and_set_operations_parse() {
        snapshot(
            "SELECT a FROM t ORDER BY b DESC NULLS FIRST, 2",
            "\
Query
  Select
    Projection
      Item
        Column a
    From
      Table t
  OrderBy
    DESC NULLS FIRST
      Column b
    ASC
      Literal 2",
        );

        snapshot(
            "SELECT a FROM t UNION ALL SELECT b FROM u",
            "\
Query
  UNION ALL
    Select
      Projection
        Item
          Column a
      From
        Table t
    Select
      Projection
        Item
          Column b
      From
        Table u",
        );

        // Set operators are left-associative and share one precedence.
        snapshot(
            "SELECT a FROM t INTERSECT SELECT b FROM u EXCEPT SELECT c FROM v",
            "\
Query
  EXCEPT
    INTERSECT
      Select
        Projection
          Item
            Column a
        From
          Table t
      Select
        Projection
          Item
            Column b
        From
          Table u
    Select
      Projection
        Item
          Column c
      From
        Table v",
        );
    }

    #[test]
    fn window_functions_parse() {
        expr_snapshot(
            "ROW_NUMBER() OVER (PARTITION BY a ORDER BY b DESC)",
            "\
Function ROW_NUMBER over
  partition by
    Column a
  order by
    Column b",
        );

        expr_snapshot(
            "SUM(x) OVER (ORDER BY y ROWS BETWEEN 2 PRECEDING AND CURRENT ROW)",
            "\
Function SUM over
  Column x
  order by
    Column y
  frame rows n preceding to current row",
        );

        // `ROWS <bound>` is shorthand for `BETWEEN <bound> AND CURRENT ROW`.
        expr_snapshot(
            "COUNT(*) OVER (ROWS UNBOUNDED PRECEDING)",
            "\
Function COUNT over
  *
  frame rows unbounded preceding to current row",
        );
    }

    #[test]
    fn subqueries_parse() {
        expr_snapshot(
            "EXISTS (SELECT 1 FROM t)",
            "\
Exists
  Query
    Select
      Projection
        Item
          Literal 1
      From
        Table t",
        );

        // `NOT EXISTS` is one operator rather than a negation of EXISTS, so
        // decorrelation can see an anti-join directly.
        expr_snapshot(
            "NOT EXISTS (SELECT 1 FROM t)",
            "\
NotExists
  Query
    Select
      Projection
        Item
          Literal 1
      From
        Table t",
        );

        expr_snapshot(
            "a NOT IN (SELECT b FROM t)",
            "\
NotInSubquery
  Column a
  Query
    Select
      Projection
        Item
          Column b
      From
        Table t",
        );

        expr_snapshot(
            "(SELECT MAX(b) FROM t)",
            "\
ScalarSubquery
  Query
    Select
      Projection
        Item
          Function MAX
            Column b
      From
        Table t",
        );
    }

    #[test]
    fn derived_tables_parse_and_require_an_alias() {
        snapshot(
            "SELECT x.a FROM (SELECT a FROM t) AS x",
            "\
Query
  Select
    Projection
      Item
        Column x.a
    From
      Derived AS x
        Query
          Select
            Projection
              Item
                Column a
            From
              Table t",
        );

        let e = parse_err("SELECT * FROM (SELECT 1)");
        assert!(e.message.contains("needs an alias"), "{}", e.message);
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        let e = parse_err("SELECT a FROM t; SELECT b FROM u");
        assert!(e.message.contains("unexpected token"));
    }
}
