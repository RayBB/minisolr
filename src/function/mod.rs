//! Function queries: the edismax `boost`, the request `boost` and `bf`, and function sorts.
//!
//! [`parse`](parse::parse) reads an expression into an [`Expr`](ast::Expr) once per
//! request; [`Expr::eval`](ast::Expr::eval) runs it per hit. [`Rescore`] applies Python
//! minisolr's rule: multiply each hit's score by every `boost` expression in turn, then
//! add each `bf` term.

pub mod ast;
pub mod eval;
pub mod parse;

use serde_json::Value;

use crate::query::Params;
use ast::Expr;

/// The functions that rescore every hit of a request.
#[derive(Debug, Default)]
pub struct Rescore<'a> {
    boosts: Vec<Expr<'a>>,
    bf: Vec<Expr<'a>>,
}

impl<'a> Rescore<'a> {
    /// Parses the `boost` expressions (multiplicative) and the `bf` value (additive; split
    /// on whitespace unless it contains `(`, as Python does).
    #[must_use]
    pub fn new(
        boosts: impl IntoIterator<Item = &'a str>,
        bf: Option<&'a str>,
        params: &'a Params,
    ) -> Self {
        let bf_terms: Vec<&str> = match bf {
            Some(v) if v.contains('(') => vec![v],
            Some(v) => v.split_whitespace().collect(),
            None => Vec::new(),
        };
        Self {
            boosts: boosts
                .into_iter()
                .map(|b| parse::parse(b, params))
                .collect(),
            bf: bf_terms
                .into_iter()
                .map(|t| parse::parse(t, params))
                .collect(),
        }
    }

    /// Whether no function changes the scores.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.boosts.is_empty() && self.bf.is_empty()
    }

    /// The final score of a hit with search score `score`; each function sees the score
    /// as rescored so far.
    #[must_use]
    pub fn apply(&self, doc: &Value, score: f64) -> f64 {
        let boosted = self.boosts.iter().fold(score, |s, b| s * b.eval(doc, s));
        self.bf.iter().fold(boosted, |s, t| s + t.eval(doc, s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn boosts_multiply_then_bf_adds() {
        let params = Params::default();
        let doc = json!({"edition_count": 4, "work_count": 30});
        let r = Rescore::new(
            ["sum(1,edition_count)", "2"],
            Some("min(work_count,20)"),
            &params,
        );
        assert!(!r.is_empty());
        assert!((r.apply(&doc, 1.5) - (1.5 * 5.0 * 2.0 + 20.0)).abs() < 1e-12);
    }

    #[test]
    fn boost_sees_running_score() {
        let params = Params::default();
        let r = Rescore::new(["score", "score"], None, &params);
        assert!(
            (r.apply(&json!({}), 3.0) - 81.0).abs() < 1e-12,
            "3*3=9, then 9*9"
        );
    }

    #[test]
    fn bf_splits_on_whitespace_without_parens() {
        let params = Params::default();
        let doc = json!({"a": 1, "b": 2});
        assert!((Rescore::new([], Some("a  b"), &params).apply(&doc, 0.5) - 3.5).abs() < 1e-12);
        assert!(
            (Rescore::new([], Some("sum(a, b)"), &params).apply(&doc, 0.5) - 3.5).abs() < 1e-12,
            "a value with ( is one term even with spaces"
        );
        assert!(Rescore::new([], None, &params).is_empty());
    }
}
