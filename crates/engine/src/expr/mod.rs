//! Expression evaluation.
//!
//! Two implementations live here, and they must always agree:
//!
//!   * [`scalar`] -- row at a time, one `ScalarValue` per value, obviously
//!     correct. It is the reference: the definition of what an expression
//!     means, and the oracle the fast path is differentially tested against.
//!   * [`vector`] -- batch at a time over whole columns, with typed kernels
//!     and bitmap logic. This is what actually runs queries.
//!
//! Keeping the scalar version rather than deleting it is deliberate. A
//! vectorized kernel is allowed to be clever only if it produces exactly what
//! the obvious implementation produces, and the only way to keep that true as
//! kernels multiply is to be able to run both.
//!
//! Three-valued logic and LIKE matching are defined once, here, and shared by
//! both. The rules, all of which follow from "NULL means unknown":
//!   * any arithmetic or comparison with a NULL operand is NULL;
//!   * `NULL AND false` is false, `NULL OR true` is true -- an unknown cannot
//!     change an already-decided answer;
//!   * `NOT NULL` is NULL;
//!   * only `TRUE` passes a filter. `NULL` and `FALSE` both fail it, which is
//!     why `WHERE x = 1` and `WHERE NOT (x = 1)` can both reject the same row.

pub mod scalar;
pub mod vector;

pub use vector::{compile, eval, eval_predicate, CompiledExpr};

use crate::plan::RelId;
use crate::types::ScalarValue;

/// Where each relation's columns sit in the incoming batch.
///
/// A column reference is bound as "column `i` of relation `r`", where `i` is an
/// index into that relation's own schema and never changes. This maps that pair
/// to a physical position in the batch -- which *does* change, as joins
/// concatenate relations side by side and projection pushdown removes columns a
/// scan no longer reads. Keeping the indirection here is what lets both happen
/// without rewriting every expression in the plan.
#[derive(Debug, Clone, Default)]
pub struct EvalContext {
    /// Per relation: the batch position of each of its columns, in the
    /// relation's own column order. `None` means the column was pruned and must
    /// not be referenced.
    rels: Vec<(RelId, Vec<Option<usize>>)>,
    /// Number of columns in the batch.
    width: usize,
}

impl EvalContext {
    pub fn empty() -> EvalContext {
        EvalContext::default()
    }

    /// A relation whose columns occupy the batch one-for-one.
    pub fn identity(rel: RelId, width: usize) -> EvalContext {
        EvalContext {
            rels: vec![(rel, (0..width).map(Some).collect())],
            width,
        }
    }

    /// A relation of `table_width` columns, of which only `projection` (in that
    /// order) reaches the batch.
    pub fn projected(rel: RelId, table_width: usize, projection: &[usize]) -> EvalContext {
        let mut columns = vec![None; table_width];
        for (position, index) in projection.iter().enumerate() {
            columns[*index] = Some(position);
        }
        EvalContext {
            rels: vec![(rel, columns)],
            width: projection.len(),
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    /// Batch position of column `index` of relation `rel`.
    pub fn position(&self, rel: RelId, index: usize) -> Option<usize> {
        self.rels
            .iter()
            .find(|(r, _)| *r == rel)
            .and_then(|(_, cols)| cols.get(index).copied().flatten())
    }

    pub fn relations(&self) -> impl Iterator<Item = RelId> + '_ {
        self.rels.iter().map(|(r, _)| *r)
    }

    /// Lay two relations side by side, which is exactly a join's output.
    pub fn concat(left: &EvalContext, right: &EvalContext) -> EvalContext {
        let mut rels = left.rels.clone();
        for (rel, cols) in &right.rels {
            rels.push((
                *rel,
                cols.iter().map(|c| c.map(|p| p + left.width)).collect(),
            ));
        }
        EvalContext {
            rels,
            width: left.width + right.width,
        }
    }
}

// -- three-valued logic ------------------------------------------------------

/// `None` is UNKNOWN.
pub fn three_valued_and(l: Option<bool>, r: Option<bool>) -> Option<bool> {
    match (l, r) {
        // A single FALSE settles the answer even if the other side is unknown.
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub fn three_valued_or(l: Option<bool>, r: Option<bool>) -> Option<bool> {
    match (l, r) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

pub fn three_valued_not(v: Option<bool>) -> Option<bool> {
    v.map(|b| !b)
}

pub fn from_tri(v: Option<bool>) -> ScalarValue {
    match v {
        Some(b) => ScalarValue::Boolean(b),
        None => ScalarValue::Null,
    }
}

// -- LIKE --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternToken {
    /// `%` -- any sequence of characters, including none.
    Any,
    /// `_` -- exactly one character.
    One,
    Literal(char),
}

fn compile_pattern(pattern: &str, escape: Option<char>) -> Vec<PatternToken> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            // An escape at the very end of the pattern has nothing to escape;
            // treat it as a literal rather than erroring.
            match chars.next() {
                Some(next) => out.push(PatternToken::Literal(next)),
                None => out.push(PatternToken::Literal(c)),
            }
            continue;
        }
        out.push(match c {
            '%' => PatternToken::Any,
            '_' => PatternToken::One,
            other => PatternToken::Literal(other),
        });
    }
    out
}

/// Greedy match with backtracking on the most recent `%`. Linear in practice
/// and quadratic only on adversarial patterns, which is the usual trade for
/// glob matching without building an automaton.
pub fn like_match(text: &str, pattern: &str, case_insensitive: bool, escape: Option<char>) -> bool {
    let (text, pattern) = if case_insensitive {
        (text.to_lowercase(), pattern.to_lowercase())
    } else {
        (text.to_string(), pattern.to_string())
    };
    let escape = if case_insensitive {
        escape.and_then(|c| c.to_lowercase().next())
    } else {
        escape
    };

    let t: Vec<char> = text.chars().collect();
    let p = compile_pattern(&pattern, escape);

    let mut ti = 0usize;
    let mut pi = 0usize;
    let mut star: Option<(usize, usize)> = None; // (pattern index, text index)

    while ti < t.len() {
        match p.get(pi) {
            Some(PatternToken::Any) => {
                star = Some((pi, ti));
                pi += 1;
                continue;
            }
            Some(PatternToken::One) => {
                pi += 1;
                ti += 1;
                continue;
            }
            Some(PatternToken::Literal(c)) if *c == t[ti] => {
                pi += 1;
                ti += 1;
                continue;
            }
            _ => {}
        }
        match star {
            // Let the last `%` swallow one more character and retry.
            Some((sp, st)) => {
                star = Some((sp, st + 1));
                ti = st + 1;
                pi = sp + 1;
            }
            None => return false,
        }
    }

    while matches!(p.get(pi), Some(PatternToken::Any)) {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_valued_truth_tables() {
        let t = Some(true);
        let f = Some(false);
        let u: Option<bool> = None;

        // AND: a FALSE anywhere wins, even against UNKNOWN.
        assert_eq!(three_valued_and(f, u), Some(false));
        assert_eq!(three_valued_and(t, u), None);
        assert_eq!(three_valued_and(u, u), None);
        assert_eq!(three_valued_and(t, t), Some(true));

        // OR: a TRUE anywhere wins.
        assert_eq!(three_valued_or(t, u), Some(true));
        assert_eq!(three_valued_or(f, u), None);
        assert_eq!(three_valued_or(f, f), Some(false));

        assert_eq!(three_valued_not(u), None);
        assert_eq!(three_valued_not(t), Some(false));
    }

    #[test]
    fn like_basics() {
        assert!(like_match("hello", "h%o", false, None));
        assert!(like_match("hello", "%", false, None));
        assert!(like_match("hello", "_ello", false, None));
        assert!(!like_match("hello", "_llo", false, None));
        assert!(!like_match("hello", "h%z", false, None));
        assert!(like_match("hello", "hello", false, None));
        assert!(!like_match("hell", "hello", false, None));
        // Backtracking case.
        assert!(like_match("abcbcd", "a%bcd", false, None));
    }

    #[test]
    fn like_is_case_sensitive_unless_ilike() {
        assert!(!like_match("Hello", "hello", false, None));
        assert!(like_match("Hello", "hello", true, None));
        assert!(like_match("HELLO", "h%O", true, None));
    }

    #[test]
    fn like_escape_makes_wildcards_literal() {
        assert!(like_match("100%", "100!%", false, Some('!')));
        assert!(!like_match("1000", "100!%", false, Some('!')));
        assert!(like_match("a_b", "a!_b", false, Some('!')));
        assert!(!like_match("axb", "a!_b", false, Some('!')));
    }

    #[test]
    fn like_handles_empty_pattern_and_text() {
        assert!(like_match("", "", false, None));
        assert!(like_match("", "%", false, None));
        assert!(!like_match("", "_", false, None));
        assert!(!like_match("a", "", false, None));
    }
}
