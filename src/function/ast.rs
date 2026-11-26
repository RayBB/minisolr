//! The parsed form of a function query such as `sum(mul(20,log(edition_count)),1)`.

/// A function of the function table, applied to its evaluated arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    /// `sum(a, ...)` / `add(a, ...)`: the sum (0 for no arguments).
    Sum,
    /// `sub(a, b)`: `a - b`.
    Sub,
    /// `mul(a, ...)` / `product(a, ...)`: the product (1 for no arguments).
    Mul,
    /// `div(a, b)`: `a / b`, or 0 when `b` is 0.
    Div,
    /// `log(a)`: base-10 logarithm, or 0 when `a <= 0`.
    Log,
    /// `ln(a)`: natural logarithm, or 0 when `a <= 0`.
    Ln,
    /// `sqrt(a)`: square root, or 0 when `a < 0`.
    Sqrt,
    /// `pow(a, b)`: `a` to the power `b`.
    Pow,
    /// `abs(a)`: absolute value.
    Abs,
    /// `min(a, ...)`: the smallest argument.
    Min,
    /// `max(a, ...)`: the largest argument.
    Max,
    /// `recip(x, m, a, b)`: `a / (m * x + b)`, or 0 when the divisor is 0.
    Recip,
    /// `if(c, a, b)`: `a` when `c` is not 0, else `b`.
    If,
}

impl Func {
    /// The function for a name, as Python minisolr's table spells it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "sum" | "add" => Self::Sum,
            "sub" => Self::Sub,
            "mul" | "product" => Self::Mul,
            "div" => Self::Div,
            "log" => Self::Log,
            "ln" => Self::Ln,
            "sqrt" => Self::Sqrt,
            "pow" => Self::Pow,
            "abs" => Self::Abs,
            "min" => Self::Min,
            "max" => Self::Max,
            "recip" => Self::Recip,
            "if" => Self::If,
            _ => return None,
        })
    }
}

/// A function query expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr<'a> {
    /// A number literal.
    Const(f64),
    /// `score`: the hit's score so far.
    Score,
    /// A field's numeric value: the first value of a multi-valued field, 0 when the field
    /// is missing or not a number.
    Field(&'a str),
    /// `def(field, default)`: the field's numeric value, or `default`.
    Def {
        /// The field name.
        field: &'a str,
        /// Evaluated when the field has no numeric value.
        default: Box<Expr<'a>>,
    },
    /// `exists(field)`: 1 when the stored document has the field, else 0.
    Exists(&'a str),
    /// A call of a function in the table.
    Call(Func, Vec<Expr<'a>>),
    /// A call of a function outside the table; evaluates to 0, as in Python minisolr.
    Unknown(&'a str),
}
