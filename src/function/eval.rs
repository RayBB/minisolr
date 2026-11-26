//! Evaluates a function query against one stored document, as Python minisolr's
//! `eval_fn` does.
//!
//! Field values come from the stored row (not the copyField-expanded one). The rows are
//! already parsed `serde_json` values, so a lookup is one map search per field reference;
//! there is no separate numeric column. Where Python would raise (a missing argument such
//! as `sub(1)`), the missing argument is 0.

use serde_json::Value;

use super::ast::{Expr, Func};

impl Expr<'_> {
    /// The value of the expression for document `doc` whose score so far is `score`.
    #[must_use]
    pub fn eval(&self, doc: &Value, score: f64) -> f64 {
        match self {
            Self::Const(v) => *v,
            Self::Score => score,
            Self::Field(name) => field_number(doc, name).unwrap_or(0.0),
            Self::Def { field, default } => {
                field_number(doc, field).unwrap_or_else(|| default.eval(doc, score))
            }
            Self::Exists(name) => {
                if doc.get(name).is_some() {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Call(func, args) => {
                let values: Vec<f64> = args.iter().map(|a| a.eval(doc, score)).collect();
                apply(*func, &values)
            }
            Self::Unknown(_) => 0.0,
        }
    }
}

/// A field's numeric value as Python's `fnum` reads it: the first element of a list, and
/// only JSON numbers count (strings, booleans and nulls do not).
#[must_use]
pub fn field_number(doc: &Value, name: &str) -> Option<f64> {
    match doc.get(name)? {
        Value::Array(items) => items.first().and_then(Value::as_f64),
        other => other.as_f64(),
    }
}

fn apply(func: Func, a: &[f64]) -> f64 {
    let arg = |i: usize| a.get(i).copied().unwrap_or(0.0);
    match func {
        Func::Sum => a.iter().fold(0.0, |acc, x| acc + x),
        Func::Mul => a.iter().fold(1.0, |acc, x| acc * x),
        Func::Sub => arg(0) - arg(1),
        Func::Div => {
            if arg(1) == 0.0 {
                0.0
            } else {
                arg(0) / arg(1)
            }
        }
        Func::Log => positive(arg(0)).map_or(0.0, f64::log10),
        Func::Ln => positive(arg(0)).map_or(0.0, f64::ln),
        Func::Sqrt => {
            if arg(0) >= 0.0 {
                arg(0).sqrt()
            } else {
                0.0
            }
        }
        Func::Pow => arg(0).powf(arg(1)),
        Func::Abs => arg(0).abs(),
        // Python's min/max keep the first of equal values and replace only on a strict win.
        Func::Min => pick(a, |x, best| x < best),
        Func::Max => pick(a, |x, best| x > best),
        Func::Recip => {
            let divisor = arg(1) * arg(0) + arg(3);
            if divisor == 0.0 {
                0.0
            } else {
                arg(2) / divisor
            }
        }
        Func::If => {
            if arg(0) == 0.0 {
                arg(2)
            } else {
                arg(1)
            }
        }
    }
}

fn positive(x: f64) -> Option<f64> {
    (x > 0.0).then_some(x)
}

fn pick(a: &[f64], better: impl Fn(f64, f64) -> bool) -> f64 {
    let mut it = a.iter().copied();
    let first = it.next().unwrap_or(0.0);
    it.fold(first, |best, x| if better(x, best) { x } else { best })
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse;
    use crate::query::Params;
    use serde_json::{Value, json};

    fn doc() -> Value {
        json!({
            "edition_count": 97,
            "readinglog_count": [12, 400],
            "ratings": 4.5,
            "title": "Alice",
            "year_str": "1865",
            "flag": true,
            "empty": [],
            "nothing": null,
        })
    }

    fn ev(expr: &str) -> f64 {
        ev_score(expr, 0.0)
    }

    fn ev_score(expr: &str, score: f64) -> f64 {
        let params = Params::new(vec![("two".into(), "2".into())]);
        parse(expr, &params).eval(&doc(), score)
    }

    fn close(expr: &str, want: f64) {
        let got = ev(expr);
        assert!((got - want).abs() < 1e-12, "{expr}: got {got}, want {want}");
    }

    #[test]
    fn constants_score_and_fields() {
        close("3", 3.0);
        assert!((ev_score("score", 1.5) - 1.5).abs() < f64::EPSILON);
        close("edition_count", 97.0);
        close("readinglog_count", 12.0);
        close("ratings", 4.5);
        close("title", 0.0);
        close("year_str", 0.0);
        close("flag", 0.0);
        close("empty", 0.0);
        close("nothing", 0.0);
        close("missing", 0.0);
        close("$two", 2.0);
    }

    #[test]
    fn arithmetic() {
        close("sum(1,2,3)", 6.0);
        close("add(1,edition_count)", 98.0);
        close("sub(10,4)", 6.0);
        close("sub(10)", 10.0);
        close("mul(2,3,4)", 24.0);
        close("product(2,ratings)", 9.0);
        close("div(9,2)", 4.5);
        close("div(9,0)", 0.0);
        close("abs(-3)", 3.0);
        close("pow(2,10)", 1024.0);
        close("sqrt(16)", 4.0);
        close("sqrt(-1)", 0.0);
    }

    #[test]
    fn logarithms() {
        close("log(1000)", 3.0);
        close("log(0)", 0.0);
        close("log(-5)", 0.0);
        close("ln(1)", 0.0);
        close("ln(-1)", 0.0);
        assert!((ev("ln(2)") - std::f64::consts::LN_2).abs() < 1e-12);
    }

    #[test]
    fn min_max_recip_if() {
        close("min(50,edition_count)", 50.0);
        close("min(5,2,8)", 2.0);
        close("max(5,2,8)", 8.0);
        close("max(edition_count,readinglog_count)", 97.0);
        close("recip(1,2,10,3)", 2.0);
        close("recip(0,1,10,0)", 0.0);
        close("if(1,7,8)", 7.0);
        close("if(missing,7,8)", 8.0);
    }

    #[test]
    fn def_exists_unknown() {
        close("def(edition_count,0)", 97.0);
        close("def(already_read_count,5)", 5.0);
        close("def(title,9999)", 9999.0);
        close("def(trending_z_score, 0)", 0.0);
        close("def(sum(1,1),9)", 2.0);
        close("exists(title)", 1.0);
        close("exists(nothing)", 1.0);
        close("exists(missing)", 0.0);
        close("ord(title)", 0.0);
        close("sum(1,ord(title))", 1.0);
    }

    #[test]
    fn book_search_boost() {
        // The BOOK_SEARCH boost from the captured requests, for edition_count=97,
        // already_read_count missing, readinglog_count=12 (first value).
        let want = 20.0 * 100f64.log10() + 0.0 + 35.0 * (16.0f64 / 4.0).log10();
        close(
            "sum(mul(20,log(sum(3,edition_count))),min(50,def(already_read_count,0)),mul(35,log(div(sum(4,def(readinglog_count,0)), 4))))",
            want,
        );
    }
}
