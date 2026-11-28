//! Parses function query strings the way Python minisolr's `eval_fn` reads them.
//!
//! Python matches `^(\w+)\((.*)\)$` (a name, then everything up to the last `)`), splits
//! the inside on commas outside parentheses, and treats anything else as `score`, a number
//! or a field name. Parsing never fails: text that is none of these is a field name, which
//! evaluates to 0 on every document. The Lucene query lexer is not reused because none of
//! its tokens (operators, ranges, escapes) mean anything here.
//!
//! One addition over Python: an argument written `$name` is replaced by the request
//! parameter `name` (Solr's parameter dereference). Python dereferences only a whole
//! edismax `boost=$name`, which the query compiler already does.

use super::ast::{Expr, Func};
use crate::query::Params;

/// Deeper nesting (or `$param` chains) than this evaluates to 0 instead of recursing.
const MAX_DEPTH: usize = 64;

/// Parses one function query expression.
#[must_use]
pub fn parse<'a>(expr: &'a str, params: &'a Params) -> Expr<'a> {
    parse_at(expr, params, 0)
}

fn parse_at<'a>(expr: &'a str, params: &'a Params, depth: usize) -> Expr<'a> {
    let expr = expr.trim();
    if depth > MAX_DEPTH {
        eprintln!("warning: function query nested too deeply, using 0: {expr}");
        return Expr::Const(0.0);
    }
    let Some((name, inner)) = split_call(expr) else {
        return leaf(expr, params, depth);
    };
    let args = split_args(inner);
    match name {
        "def" => {
            let first = args.first().map_or("", |a| a.trim());
            if is_field_name(first) {
                let default = args
                    .get(1)
                    .map_or(Expr::Const(0.0), |d| parse_at(d, params, depth + 1));
                Expr::Def {
                    field: first,
                    default: Box::new(default),
                }
            } else {
                // Python evaluates a non-field first argument and ignores the default.
                parse_at(first, params, depth + 1)
            }
        }
        "exists" => Expr::Exists(args.first().map_or("", |a| a.trim())),
        _ => {
            let Some(func) = Func::from_name(name) else {
                crate::warn::note(
                    &format!("function:{name}"),
                    &format!("function {name}() evaluates to 0"),
                );
                return Expr::Unknown(name);
            };
            let args = args.into_iter().map(|a| parse_at(a, params, depth + 1));
            Expr::Call(func, args.collect())
        }
    }
}

fn leaf<'a>(text: &'a str, params: &'a Params, depth: usize) -> Expr<'a> {
    if text == "score" {
        return Expr::Score;
    }
    if let Ok(v) = text.parse::<f64>() {
        return Expr::Const(v);
    }
    if let Some(value) = text.strip_prefix('$').and_then(|name| params.get(name)) {
        return parse_at(value, params, depth + 1);
    }
    Expr::Field(text)
}

/// `name(inner)` when `expr` is a word followed by `(` and ends with `)`.
fn split_call(expr: &str) -> Option<(&str, &str)> {
    let open = expr.find(|c: char| !is_word_char(c))?;
    let (name, rest) = expr.split_at(open);
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    (!name.is_empty()).then_some((name, inner))
}

/// Splits on commas at parenthesis depth 0 (a stray `)` makes the depth negative, as in
/// Python, so later commas stay unsplit).
fn split_args(inner: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                args.push(inner.get(start..i).unwrap_or_default());
                start = i + 1;
            }
            _ => {}
        }
    }
    args.push(inner.get(start..).unwrap_or_default());
    args
}

fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Python's `[\w.]+`.
fn is_field_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| is_word_char(c) || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Expr<'_> {
        static EMPTY: std::sync::OnceLock<Params> = std::sync::OnceLock::new();
        parse(s, EMPTY.get_or_init(Params::default))
    }

    #[test]
    fn leaves() {
        assert_eq!(p(" 2.5 "), Expr::Const(2.5));
        assert_eq!(p("-1e2"), Expr::Const(-100.0));
        assert_eq!(p("score"), Expr::Score);
        assert_eq!(p("edition_count"), Expr::Field("edition_count"));
        assert_eq!(p(""), Expr::Field(""));
        assert_eq!(
            p("foo(bar"),
            Expr::Field("foo(bar"),
            "unbalanced is a field name"
        );
    }

    #[test]
    fn nested_calls() {
        assert_eq!(
            p("sum(mul(20, log(sum(3,edition_count))), 1)"),
            Expr::Call(
                Func::Sum,
                vec![
                    Expr::Call(
                        Func::Mul,
                        vec![
                            Expr::Const(20.0),
                            Expr::Call(
                                Func::Log,
                                vec![Expr::Call(
                                    Func::Sum,
                                    vec![Expr::Const(3.0), Expr::Field("edition_count")]
                                )]
                            )
                        ]
                    ),
                    Expr::Const(1.0)
                ]
            )
        );
        assert_eq!(p("product()"), Expr::Call(Func::Mul, vec![Expr::Field("")]));
    }

    #[test]
    fn def_and_exists() {
        assert_eq!(
            p("def(trending_z_score, 0)"),
            Expr::Def {
                field: "trending_z_score",
                default: Box::new(Expr::Const(0.0))
            }
        );
        assert_eq!(
            p("def(sum(1,2), 9)"),
            Expr::Call(Func::Sum, vec![Expr::Const(1.0), Expr::Const(2.0)]),
            "a non-field first argument is evaluated, the default ignored"
        );
        assert_eq!(
            p("def(a.b)"),
            Expr::Def {
                field: "a.b",
                default: Box::new(Expr::Const(0.0))
            }
        );
        assert_eq!(p("exists( ia )"), Expr::Exists("ia"));
    }

    #[test]
    fn unknown_function() {
        assert_eq!(p("ord(title)"), Expr::Unknown("ord"));
    }

    #[test]
    fn param_dereference() {
        let params = Params::new(vec![
            ("b".into(), "log(edition_count)".into()),
            ("loop".into(), "$loop".into()),
        ]);
        assert_eq!(
            parse("mul(2,$b)", &params),
            Expr::Call(
                Func::Mul,
                vec![
                    Expr::Const(2.0),
                    Expr::Call(Func::Log, vec![Expr::Field("edition_count")])
                ]
            )
        );
        assert_eq!(parse("$missing", &params), Expr::Field("$missing"));
        assert_eq!(parse("$loop", &params), Expr::Const(0.0), "cycles stop");
    }

    #[test]
    fn split_args_respects_depth() {
        assert_eq!(split_args("a,f(b,c),d"), ["a", "f(b,c)", "d"]);
        assert_eq!(split_args("a),b,c"), ["a),b,c"]);
        assert_eq!(split_args(""), [""]);
    }
}
