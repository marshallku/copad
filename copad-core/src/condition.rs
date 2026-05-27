//! Boolean expression DSL for trigger `condition` clauses.
//!
//! Evaluated against the firing `Event` payload + the current
//! `Context`. Exists for patterns the positive-only payload-match in
//! `WhenSpec` cannot express:
//! - "skip if I declined": `event.my_response_status != "declined"`
//! - "skip the weekly 1:1": `event.recurring_id != "weekly-1on1"`
//! - "only physical-location meetings": `event.location != null`
//! - combined: `event.my_response_status != "declined" && event.recurring_id != "x"`
//!
//! Grammar (recursive descent, left-associative for `&&` / `||`):
//! ```text
//! expr     = or_expr
//! or_expr  = and_expr ( '||' and_expr )*
//! and_expr = not_expr ( '&&' not_expr )*
//! not_expr = '!' not_expr | cmp_expr
//! cmp_expr = atom ( CMPOP atom )?      // bare atom must eval to bool
//! atom     = ref | string | number | bool | null | '(' expr ')'
//! ref      = IDENT ( '.' IDENT )+      // event.id, context.active_panel
//! CMPOP    = '==' | '!=' | '<' | '<=' | '>' | '>='
//! ```
//!
//! Eval semantics:
//! - `==` / `!=` are type-tolerant (`serde_json::Value` equality —
//!   `event.x == 1` matches both `1` and `1.0`; `event.x == "1"` does
//!   not match a numeric `1`).
//! - Ordering ops (`<`, `<=`, `>`, `>=`) require both sides to be
//!   numbers; non-numeric operands return an evaluation error which
//!   the caller (TriggerEngine) treats as "trigger does not match"
//!   plus a `log::warn`.
//! - `event.X.Y` traverses the JSON payload by key. A missing path
//!   resolves to `null` (so `event.missing_field == "x"` is `false`,
//!   not an error).
//! - `context.active_panel` / `context.active_cwd` / `context.presence`
//!   are the supported context paths (no nesting; matches the
//!   `{context.X}` interpolation surface). `context.presence` resolves
//!   to the string `"active"` or `"away"`.
//! - `&&` / `||` short-circuit; both sides must produce bool when
//!   actually evaluated, else error.
//! - `!` negates a bool, else error.
//! - A bare ref / literal at the top level must produce a bool
//!   (`event.is_priority`, or `true` / `false`).

use crate::context::Context;
use crate::event_bus::Event;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(CmpOp, Atom, Atom),
    /// Bare atom — must evaluate to a bool at runtime.
    Atom(Atom),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Atom {
    /// Dotted path: first segment is `event` or `context`, remainder
    /// is the key path inside the event payload or the context.
    Ref(Vec<String>),
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
    /// `(...)` — overrides default `&&`-tighter-than-`||` precedence.
    Group(Box<Expr>),
}

// -- Public API --------------------------------------------------------

/// Parse a condition source string into an AST. Errors carry a
/// human-readable message suitable for `log::warn` output.
pub fn parse(src: &str) -> Result<Expr, String> {
    let tokens = lex(src)?;
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_or()?;
    if p.pos < p.tokens.len() {
        return Err(format!(
            "unexpected trailing input at position {}: {:?}",
            p.pos, p.tokens[p.pos]
        ));
    }
    Ok(expr)
}

/// `Err` is treated as "condition not satisfied" by the caller (after
/// logging) — see `TriggerEngine::dispatch`.
pub fn eval(expr: &Expr, event: &Event, context: Option<&Context>) -> Result<bool, String> {
    let v = eval_expr(expr, event, context)?;
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(format!(
            "top-level condition must produce a bool, got {other}"
        )),
    }
}

// -- Lexer -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    True,
    False,
    Null,
    Eq,  // ==
    Neq, // !=
    Lt,  // <
    Le,  // <=
    Gt,  // >
    Ge,  // >=
    And, // &&
    Or,  // ||
    Not, // !
    Dot, // .
    LParen,
    RParen,
}

fn lex(src: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            b'.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            b'=' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Eq);
                    i += 2;
                } else {
                    return Err(format!("expected `==` at position {i}, got `=`"));
                }
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Neq);
                    i += 2;
                } else {
                    tokens.push(Token::Not);
                    i += 1;
                }
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Le);
                    i += 2;
                } else {
                    tokens.push(Token::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token::Ge);
                    i += 2;
                } else {
                    tokens.push(Token::Gt);
                    i += 1;
                }
            }
            b'&' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    tokens.push(Token::And);
                    i += 2;
                } else {
                    return Err(format!("expected `&&` at position {i}"));
                }
            }
            b'|' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    tokens.push(Token::Or);
                    i += 2;
                } else {
                    return Err(format!("expected `||` at position {i}"));
                }
            }
            b'"' => {
                let (s, consumed) = lex_string(&bytes[i..])
                    .map_err(|e| format!("string literal at position {i}: {e}"))?;
                tokens.push(Token::Str(s));
                i += consumed;
            }
            b'-' | b'0'..=b'9' => {
                let (n, consumed) =
                    lex_number(&bytes[i..]).map_err(|e| format!("number at position {i}: {e}"))?;
                tokens.push(Token::Num(n));
                i += consumed;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let (ident, consumed) = lex_ident(&bytes[i..]);
                tokens.push(match ident.as_str() {
                    "true" => Token::True,
                    "false" => Token::False,
                    "null" => Token::Null,
                    _ => Token::Ident(ident),
                });
                i += consumed;
            }
            _ => {
                return Err(format!(
                    "unexpected character `{}` at position {i}",
                    c as char
                ));
            }
        }
    }
    Ok(tokens)
}

fn lex_string(bytes: &[u8]) -> Result<(String, usize), String> {
    debug_assert_eq!(bytes[0], b'"');
    // Collect raw bytes (NOT chars) so multi-byte UTF-8 sequences
    // pass through intact. The previous implementation pushed each
    // byte as `as char`, which mangled non-ASCII content like emoji
    // — `"📝"` (4 bytes) became 4 garbage replacement chars, so
    // `event.emoji_name == "📝"` never matched. Final conversion
    // via `String::from_utf8` is infallible whenever the source
    // string was valid UTF-8 (which it always is — `lex` takes
    // `&str`), but we surface the error path defensively.
    let mut out: Vec<u8> = Vec::new();
    let mut i = 1; // skip opening quote
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let s = String::from_utf8(out)
                    .map_err(|e| format!("invalid utf-8 in string literal: {e}"))?;
                return Ok((s, i + 1));
            }
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return Err("unterminated escape".into());
                }
                let esc = bytes[i + 1];
                let c: u8 = match esc {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    other => return Err(format!("unknown escape `\\{}`", other as char)),
                };
                out.push(c);
                i += 2;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    Err("unterminated string".into())
}

fn lex_number(bytes: &[u8]) -> Result<(f64, usize), String> {
    let mut i = 0;
    if bytes[0] == b'-' {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == start {
        return Err("no digits".into());
    }
    let s = std::str::from_utf8(&bytes[..i]).map_err(|e| e.to_string())?;
    let n: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    Ok((n, i))
}

fn lex_ident(bytes: &[u8]) -> (String, usize) {
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphanumeric() || c == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    (
        std::str::from_utf8(&bytes[..i]).unwrap_or("").to_string(),
        i,
    )
}

// -- Parser ------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_not()?;
        while matches!(self.peek(), Some(Token::And)) {
            self.bump();
            let rhs = self.parse_not()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if matches!(self.peek(), Some(Token::Not)) {
            self.bump();
            let inner = self.parse_not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_atom()?;
        let op = match self.peek() {
            Some(Token::Eq) => Some(CmpOp::Eq),
            Some(Token::Neq) => Some(CmpOp::Neq),
            Some(Token::Lt) => Some(CmpOp::Lt),
            Some(Token::Le) => Some(CmpOp::Le),
            Some(Token::Gt) => Some(CmpOp::Gt),
            Some(Token::Ge) => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let rhs = self.parse_atom()?;
            Ok(Expr::Cmp(op, lhs, rhs))
        } else {
            Ok(Expr::Atom(lhs))
        }
    }

    fn parse_atom(&mut self) -> Result<Atom, String> {
        match self.bump() {
            Some(Token::Str(s)) => Ok(Atom::Str(s)),
            Some(Token::Num(n)) => Ok(Atom::Num(n)),
            Some(Token::True) => Ok(Atom::Bool(true)),
            Some(Token::False) => Ok(Atom::Bool(false)),
            Some(Token::Null) => Ok(Atom::Null),
            Some(Token::LParen) => {
                let inner = self.parse_or()?;
                match self.bump() {
                    Some(Token::RParen) => Ok(Atom::Group(Box::new(inner))),
                    other => Err(format!("expected `)`, got {other:?}")),
                }
            }
            Some(Token::Ident(first)) => {
                // Must be followed by `.<ident>` at least once. We do
                // NOT support bare top-level identifiers (`foo` by
                // itself) so a typo like `recurring_id` instead of
                // `event.recurring_id` errors loudly instead of being
                // silently treated as an unbound name.
                let mut path = vec![first];
                let mut had_dot = false;
                while matches!(self.peek(), Some(Token::Dot)) {
                    self.bump();
                    had_dot = true;
                    match self.bump() {
                        Some(Token::Ident(s)) => path.push(s),
                        other => {
                            return Err(format!("expected identifier after `.`, got {other:?}"));
                        }
                    }
                }
                if !had_dot {
                    return Err(format!(
                        "bare identifier `{}` is not a reference; use `event.<field>` \
                         or `context.<field>` (or quote the literal)",
                        path[0]
                    ));
                }
                if path[0] != "event" && path[0] != "context" {
                    return Err(format!(
                        "reference root must be `event` or `context`, got `{}`",
                        path[0]
                    ));
                }
                Ok(Atom::Ref(path))
            }
            Some(other) => Err(format!("unexpected token {other:?}")),
            None => Err("unexpected end of expression".into()),
        }
    }
}

// -- Evaluator ---------------------------------------------------------

fn eval_expr(expr: &Expr, event: &Event, context: Option<&Context>) -> Result<Value, String> {
    match expr {
        Expr::Or(a, b) => {
            let lv = eval_expr(a, event, context)?;
            let lb = as_bool(&lv).map_err(|e| format!("`||` lhs: {e}"))?;
            if lb {
                return Ok(Value::Bool(true));
            }
            let rv = eval_expr(b, event, context)?;
            let rb = as_bool(&rv).map_err(|e| format!("`||` rhs: {e}"))?;
            Ok(Value::Bool(rb))
        }
        Expr::And(a, b) => {
            let lv = eval_expr(a, event, context)?;
            let lb = as_bool(&lv).map_err(|e| format!("`&&` lhs: {e}"))?;
            if !lb {
                return Ok(Value::Bool(false));
            }
            let rv = eval_expr(b, event, context)?;
            let rb = as_bool(&rv).map_err(|e| format!("`&&` rhs: {e}"))?;
            Ok(Value::Bool(rb))
        }
        Expr::Not(e) => {
            let v = eval_expr(e, event, context)?;
            let b = as_bool(&v).map_err(|e| format!("`!` operand: {e}"))?;
            Ok(Value::Bool(!b))
        }
        Expr::Cmp(op, a, b) => {
            let lv = eval_atom(a, event, context)?;
            let rv = eval_atom(b, event, context)?;
            Ok(Value::Bool(eval_cmp(*op, &lv, &rv)?))
        }
        Expr::Atom(a) => eval_atom(a, event, context),
    }
}

fn eval_atom(atom: &Atom, event: &Event, context: Option<&Context>) -> Result<Value, String> {
    match atom {
        Atom::Str(s) => Ok(Value::String(s.clone())),
        Atom::Num(n) => Ok(serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null)),
        Atom::Bool(b) => Ok(Value::Bool(*b)),
        Atom::Null => Ok(Value::Null),
        Atom::Ref(path) => Ok(resolve_ref(path, event, context)),
        Atom::Group(e) => eval_expr(e, event, context),
    }
}

fn resolve_ref(path: &[String], event: &Event, context: Option<&Context>) -> Value {
    if path.len() < 2 {
        return Value::Null;
    }
    match path[0].as_str() {
        "event" => {
            let mut cur = &event.payload;
            for seg in &path[1..] {
                match cur.get(seg) {
                    Some(v) => cur = v,
                    None => return Value::Null,
                }
            }
            cur.clone()
        }
        "context" => {
            let Some(ctx) = context else {
                return Value::Null;
            };
            // We keep the surface narrow — only the existing
            // `{context.X}` interpolation fields are supported.
            // Nesting is not — `context.active_panel.something`
            // resolves to Null.
            if path.len() != 2 {
                return Value::Null;
            }
            match path[1].as_str() {
                "active_panel" => ctx
                    .active_panel
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                "active_cwd" => ctx
                    .active_cwd
                    .as_ref()
                    .map(|p| Value::String(p.to_string_lossy().to_string()))
                    .unwrap_or(Value::Null),
                "presence" => Value::String(ctx.presence.as_str().to_string()),
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    }
}

fn eval_cmp(op: CmpOp, lhs: &Value, rhs: &Value) -> Result<bool, String> {
    match op {
        CmpOp::Eq => Ok(values_eq(lhs, rhs)),
        CmpOp::Neq => Ok(!values_eq(lhs, rhs)),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            let ln = as_f64(lhs)
                .ok_or_else(|| format!("ordering op requires numeric operands, got lhs={lhs}"))?;
            let rn = as_f64(rhs)
                .ok_or_else(|| format!("ordering op requires numeric operands, got rhs={rhs}"))?;
            Ok(match op {
                CmpOp::Lt => ln < rn,
                CmpOp::Le => ln <= rn,
                CmpOp::Gt => ln > rn,
                CmpOp::Ge => ln >= rn,
                _ => unreachable!(),
            })
        }
    }
}

/// `serde_json::Value::PartialEq` returns false for `1 == 1.0`. Trigger
/// authors expect integer-vs-float to compare equal, so we normalize
/// both to `f64` for the numeric case.
fn values_eq(a: &Value, b: &Value) -> bool {
    if let (Some(an), Some(bn)) = (a.as_f64(), b.as_f64()) {
        return an == bn;
    }
    a == b
}

fn as_bool(v: &Value) -> Result<bool, String> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(format!("expected bool, got {other}")),
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::Event;
    use serde_json::json;
    use std::path::PathBuf;

    fn evt(payload: Value) -> Event {
        Event::new("k", "test", payload)
    }

    fn ctx(panel: Option<&str>, cwd: Option<&str>) -> Context {
        Context {
            active_panel: panel.map(str::to_string),
            active_cwd: cwd.map(PathBuf::from),
            ..Default::default()
        }
    }

    fn p_eval(src: &str, event: &Event, context: Option<&Context>) -> Result<bool, String> {
        let expr = parse(src)?;
        eval(&expr, event, context)
    }

    // -- Parser --

    #[test]
    fn parses_simple_eq() {
        let _ = parse(r#"event.x == "hello""#).unwrap();
    }

    #[test]
    fn parses_complex() {
        let _ = parse(
            r#"event.my_response_status != "declined" && event.recurring_id != "weekly-1on1""#,
        )
        .unwrap();
    }

    #[test]
    fn rejects_bare_identifier() {
        let err = parse("foo == 1").unwrap_err();
        assert!(err.contains("bare identifier"), "got {err}");
    }

    #[test]
    fn rejects_unknown_root() {
        let err = parse("x.foo == 1").unwrap_err();
        assert!(err.contains("reference root"), "got {err}");
    }

    #[test]
    fn rejects_trailing_garbage() {
        let err = parse("event.x == 1 garbage").unwrap_err();
        assert!(err.contains("trailing input") || err.contains("bare identifier"));
    }

    #[test]
    fn rejects_unclosed_paren() {
        let err = parse("(event.x == 1").unwrap_err();
        assert!(err.contains("`)`") || err.contains("end of expression"));
    }

    #[test]
    fn rejects_unterminated_string() {
        let err = parse(r#"event.x == "abc"#).unwrap_err();
        assert!(err.contains("unterminated"));
    }

    // -- Eval: equality --

    #[test]
    fn eq_matches_string() {
        let e = evt(json!({"name": "alice"}));
        assert!(p_eval(r#"event.name == "alice""#, &e, None).unwrap());
        assert!(!p_eval(r#"event.name == "bob""#, &e, None).unwrap());
    }

    #[test]
    fn neq_skips_match() {
        let e = evt(json!({"status": "declined"}));
        assert!(!p_eval(r#"event.status != "declined""#, &e, None).unwrap());
        let e2 = evt(json!({"status": "accepted"}));
        assert!(p_eval(r#"event.status != "declined""#, &e2, None).unwrap());
    }

    #[test]
    fn missing_path_resolves_to_null() {
        // event.absent == "x"  →  null == "x"  →  false (no error)
        let e = evt(json!({"present": 1}));
        assert!(!p_eval(r#"event.absent == "x""#, &e, None).unwrap());
        // event.absent == null  →  true
        assert!(p_eval(r#"event.absent == null"#, &e, None).unwrap());
    }

    #[test]
    fn nested_path_traverses_payload() {
        let e = evt(json!({"organizer": {"email": "x@y.com"}}));
        assert!(p_eval(r#"event.organizer.email == "x@y.com""#, &e, None).unwrap());
        assert!(!p_eval(r#"event.organizer.email == "z@y.com""#, &e, None).unwrap());
    }

    // -- Eval: ordering --

    #[test]
    fn ordering_on_numbers() {
        let e = evt(json!({"n": 5}));
        assert!(p_eval("event.n > 3", &e, None).unwrap());
        assert!(p_eval("event.n >= 5", &e, None).unwrap());
        assert!(!p_eval("event.n > 5", &e, None).unwrap());
        assert!(p_eval("event.n < 6", &e, None).unwrap());
    }

    #[test]
    fn ordering_on_non_numeric_errors() {
        let e = evt(json!({"s": "hello"}));
        let err = p_eval(r#"event.s > "abc""#, &e, None).unwrap_err();
        assert!(err.contains("ordering op"), "got {err}");
    }

    // -- Eval: logical --

    #[test]
    fn and_short_circuits_false() {
        // If lhs is false, rhs is not evaluated. We test this by
        // making rhs an expression that would error if evaluated
        // (ordering on non-numeric).
        let e = evt(json!({"x": 1, "s": "a"}));
        // lhs false → and-result false, rhs (which would error) is never reached
        let result = p_eval(r#"event.x == 99 && event.s > "b""#, &e, None).unwrap();
        assert!(!result);
    }

    #[test]
    fn or_short_circuits_true() {
        let e = evt(json!({"x": 1, "s": "a"}));
        // lhs true → or-result true; rhs (would error) skipped.
        let result = p_eval(r#"event.x == 1 || event.s > "b""#, &e, None).unwrap();
        assert!(result);
    }

    #[test]
    fn not_negates_bool() {
        let e = evt(json!({"x": 1}));
        assert!(p_eval("!(event.x == 2)", &e, None).unwrap());
        assert!(!p_eval("!(event.x == 1)", &e, None).unwrap());
    }

    #[test]
    fn parens_group_correctly() {
        let e = evt(json!({"a": 1, "b": 2, "c": 3}));
        // Without parens: a==1 && b==99 || c==3 → (a==1 && b==99) || c==3 → false || true → true
        assert!(p_eval("event.a == 1 && event.b == 99 || event.c == 3", &e, None).unwrap());
        // With parens forcing other association:
        assert!(!p_eval("event.a == 1 && (event.b == 99 || event.c == 99)", &e, None).unwrap());
    }

    // -- Eval: context --

    #[test]
    fn context_active_panel_resolves() {
        let e = evt(json!({}));
        let c = ctx(Some("panel-1"), None);
        assert!(p_eval(r#"context.active_panel == "panel-1""#, &e, Some(&c)).unwrap());
    }

    #[test]
    fn context_missing_resolves_null() {
        let e = evt(json!({}));
        let c = ctx(None, None);
        assert!(p_eval("context.active_panel == null", &e, Some(&c)).unwrap());
    }

    #[test]
    fn context_presence_resolves_active_by_default() {
        let e = evt(json!({}));
        let c = ctx(None, None);
        assert!(p_eval(r#"context.presence == "active""#, &e, Some(&c)).unwrap());
        assert!(!p_eval(r#"context.presence == "away""#, &e, Some(&c)).unwrap());
    }

    #[test]
    fn context_presence_away_matches_string_literal() {
        let e = evt(json!({}));
        let mut c = ctx(None, None);
        c.presence = crate::context::Presence::Away;
        assert!(p_eval(r#"context.presence == "away""#, &e, Some(&c)).unwrap());
        assert!(!p_eval(r#"context.presence == "active""#, &e, Some(&c)).unwrap());
    }

    #[test]
    fn context_unknown_field_is_null() {
        let e = evt(json!({}));
        let c = ctx(Some("x"), None);
        assert!(p_eval("context.no_such_field == null", &e, Some(&c)).unwrap());
    }

    #[test]
    fn no_context_treats_all_context_refs_as_null() {
        let e = evt(json!({}));
        assert!(p_eval("context.active_panel == null", &e, None).unwrap());
    }

    // -- Eval: top-level type --

    #[test]
    fn bare_atom_must_be_bool() {
        // event.flag is bool — fine
        let e = evt(json!({"flag": true, "n": 5}));
        assert!(p_eval("event.flag", &e, None).unwrap());
        // bare number → error
        let err = p_eval("event.n", &e, None).unwrap_err();
        assert!(err.contains("must produce a bool"), "got {err}");
    }

    #[test]
    fn literal_true_evaluates_true() {
        let e = evt(json!({}));
        assert!(p_eval("true", &e, None).unwrap());
        assert!(!p_eval("false", &e, None).unwrap());
    }

    // -- Real-world usage patterns --

    #[test]
    fn skip_declined_pattern() {
        // Trigger should NOT fire when my_response_status is declined.
        let e1 = evt(json!({"my_response_status": "declined"}));
        let e2 = evt(json!({"my_response_status": "accepted"}));
        let cond = r#"event.my_response_status != "declined""#;
        assert!(!p_eval(cond, &e1, None).unwrap());
        assert!(p_eval(cond, &e2, None).unwrap());
    }

    #[test]
    fn skip_one_recurring_event_pattern() {
        let e_1on1 = evt(json!({"recurring_id": "weekly-1on1"}));
        let e_other = evt(json!({"recurring_id": "team-sync"}));
        let cond = r#"event.recurring_id != "weekly-1on1""#;
        assert!(!p_eval(cond, &e_1on1, None).unwrap());
        assert!(p_eval(cond, &e_other, None).unwrap());
    }

    #[test]
    fn combined_skip_declined_and_skip_1on1() {
        let cond =
            r#"event.my_response_status != "declined" && event.recurring_id != "weekly-1on1""#;
        // accepted, not 1on1 → fire
        assert!(
            p_eval(
                cond,
                &evt(json!({"my_response_status": "accepted", "recurring_id": "team-sync"})),
                None
            )
            .unwrap()
        );
        // declined, not 1on1 → skip
        assert!(
            !p_eval(
                cond,
                &evt(json!({"my_response_status": "declined", "recurring_id": "team-sync"})),
                None
            )
            .unwrap()
        );
        // accepted, IS 1on1 → skip
        assert!(
            !p_eval(
                cond,
                &evt(json!({"my_response_status": "accepted", "recurring_id": "weekly-1on1"})),
                None
            )
            .unwrap()
        );
    }

    // -- Eval: unicode string literals (regression for the
    // `event.emoji_name == "📝"` reaction-trigger case where the
    // byte-level lexer mangled multi-byte UTF-8 into per-byte
    // garbage chars). --

    #[test]
    fn string_literal_preserves_multibyte_utf8() {
        // 📝 is U+1F4DD = 4 bytes in UTF-8 (F0 9F 93 9D). Before the
        // fix the lexer would tokenize that as 4 separate chars and
        // never equal the single-codepoint payload value.
        let e = evt(json!({"emoji_name": "📝"}));
        assert!(p_eval(r#"event.emoji_name == "📝""#, &e, None).unwrap());
        let e2 = evt(json!({"emoji_name": "🔥"}));
        assert!(!p_eval(r#"event.emoji_name == "📝""#, &e2, None).unwrap());
    }

    #[test]
    fn string_literal_preserves_korean_text() {
        // Hangul syllables are 3-byte UTF-8 — same lexer bug would
        // hit them. Smoke-test the general case.
        let e = evt(json!({"label": "안녕"}));
        assert!(p_eval(r#"event.label == "안녕""#, &e, None).unwrap());
    }

    #[test]
    fn string_literal_with_escape_then_unicode() {
        // Escape handling and unicode pass-through must compose.
        let e = evt(json!({"text": "\"📝\""}));
        assert!(p_eval(r#"event.text == "\"📝\"""#, &e, None).unwrap());
    }
}
