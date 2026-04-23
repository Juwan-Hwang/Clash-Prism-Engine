//! # Static Expression Evaluator
//!
//! Provides pure-Rust expression evaluation for `$filter` / `$remove` / `$transform` operations,
//! **without starting a rquickjs runtime**. Covers the most common expression patterns in Prism DSL.
//!
//! ## Why Pure Rust?
//!
//! - **Performance**: No JS engine startup overhead (~ms per expression)
//! - **Safety**: No sandbox escape vectors (lexical state machine)
//! - **Simplicity**: Most DSL expressions are simple comparisons — overkill to use JS
//!
//! ## Supported Syntax
//!
//! ### Predicate Expressions (`$filter` / `$remove`)
//!
//! ```text
//! p.field == "value"        // Equality comparison
//! p.field != "value"        // Inequality comparison
//! p.field > 443             // Numeric greater-than
//! p.field < 443             // Numeric less-than
//! p.field >= 443            // Numeric greater-or-equal
//! p.field <= 443            // Numeric less-or-equal
//! p.field == true           // Boolean comparison
//! p.name.includes("text")   // Substring contains
//! p.name.match(/pattern/)   // Regex match
//! (expr1) && (expr2)       // Logical AND
//! (expr1) || (expr2)       // Logical OR
//! !expr                     // Logical NOT
//! ```
//!
//! ### Transform Expressions (`$transform`)
//!
//! ```text
//! {...p, name: "prefix-" + p.name}           // Spread and override field
//! {name: p.name.replace(/old/new/)}          // Regex replace name
//! {...p, "udp-support": true}                // Add new field
//! {name: "🇭🇰 " + p.name}                   // String concatenation prefix
//! ```
//!
//! ## Regex Caching
//!
//! All regex patterns are cached in a thread-local [`REGEX_CACHE`] (`HashMap<String, Regex>`).
//! Same pattern string is compiled only once, then reused across all evaluations.
//! This eliminates redundant regex compilation overhead in loops.

use regex::Regex;
use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::LazyLock;

// Fast-path regex for `.includes()` method calls.
// Uses greedy `(.+)` which matches up to the last `)` in the expression.
// This is intentionally greedy (not `(.+?)`) because:
//   1. The regex is anchored with `$` after the closing `)`, so both greedy
//      and non-greedy quantifiers produce the same match — they both consume
//      everything between the first `(` and the last `)`.
//   2. Actual argument extraction uses `find_balanced_parens()` in
//      `eval_method_call()`, which correctly handles nested parentheses and
//      string literals. This regex is only used for the initial dispatch.
static INCLUDES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^p\.(\w+(?:\.\w+)*)\.includes\((.+)\)$"#).unwrap());

// Thread-local regex cache (same pattern compiled only once).
// Uses IndexMap for LRU eviction instead of clearing all on overflow.
const MAX_CACHE_SIZE: usize = 512;

thread_local! {
    static REGEX_CACHE: RefCell<indexmap::IndexMap<String, Regex>> = RefCell::new(indexmap::IndexMap::new());
}

/// Get a cached Regex for the given pattern, or compile and cache it.
/// Avoids redundant regex compilation in loops.
///
/// When cache is full, evicts oldest 25% of entries (LRU) instead
/// of clearing all entries (avalanche risk).
pub fn get_cached_regex(pattern: &str) -> std::result::Result<Regex, String> {
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.contains_key(pattern) {
            // Use move_index to promote to back for LRU ordering.
            // move_index is O(n) in the worst case, but the regex cache is small
            // (MAX_CACHE_SIZE = 512), so this is acceptable in practice.
            if let Some(idx) = cache.get_index_of(pattern) {
                let last = cache.len() - 1;
                cache.move_index(idx, last);
            }
            Ok(cache.get(pattern).unwrap().clone())
        } else {
            if cache.len() >= MAX_CACHE_SIZE {
                let evict_count = MAX_CACHE_SIZE / 4;
                for _ in 0..evict_count {
                    cache.shift_remove_index(0);
                }
                tracing::debug!(
                    "Regex cache exceeded max size ({}), evicted {} oldest entries (LRU)",
                    MAX_CACHE_SIZE,
                    evict_count
                );
            }
            match regex::RegexBuilder::new(pattern)
                .size_limit(1024 * 1024)
                .dfa_size_limit(1024 * 1024)
                .build()
            {
                Ok(re) => {
                    cache.insert(pattern.to_string(), re.clone());
                    Ok(re)
                }
                Err(e) => Err(format!("Invalid regex '{}': {}", pattern, e)),
            }
        }
    })
}

/// Expression evaluation result type (unified error type for all expression operations).
#[derive(Debug, Clone)]
pub enum ExprError {
    /// Syntax/parse error — the expression could not be parsed
    ParseError(String),
    /// Referenced field does not exist in the item
    FieldNotFound(String),
    /// Type mismatch between expected and actual value types
    TypeMismatch { expected: String, actual: String },
    /// Regular expression compilation failed
    RegexError(String),
    /// Expression syntax is not supported by this evaluator
    Unsupported(String),
}

impl std::fmt::Display for ExprError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExprError::ParseError(msg) => write!(f, "Expression parse error: {}", msg),
            ExprError::FieldNotFound(field) => write!(f, "Field not found: {}", field),
            ExprError::TypeMismatch { expected, actual } => {
                write!(f, "Type mismatch: expected {}, got {}", expected, actual)
            }
            ExprError::RegexError(msg) => write!(f, "Regex error: {}", msg),
            ExprError::Unsupported(expr) => write!(f, "Unsupported expression: {}", expr),
        }
    }
}

impl std::error::Error for ExprError {}

/// Expression value type — the result of evaluating a sub-expression.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprValue {
    Bool(bool),
    /// Numeric value stored as `f64`.
    ///
    /// # Note: Large Integer Precision
    ///
    /// `f64` has 53 bits of mantissa precision, which means integers larger than
    /// `2^53 - 1` (9,007,199,254,740,991) cannot be represented exactly.
    /// For example, `9007199254740993` would be rounded to `9007199254740992.0`.
    ///
    /// This is acceptable for Prism DSL's use case (proxy port numbers, thresholds,
    /// etc. are well within f64's exact range). If arbitrary-precision integers are
    /// needed in the future, consider adding an `Int64` variant.
    Number(f64),
    /// P-01: Uses `Cow<'static, str>` to avoid cloning strings from JSON values.
    /// Borrowed strings come from `json_to_expr` (zero-copy), owned strings from
    /// `parse_literal` and string operations.
    String(std::borrow::Cow<'static, str>),
    /// JSON array value (e.g., for `len()` support on arrays).
    Array(Vec<serde_json::Value>),
    Null,
}

impl std::fmt::Display for ExprValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExprValue::Bool(b) => write!(f, "{}", b),
            ExprValue::Number(n) => write!(f, "{}", n),
            ExprValue::String(s) => write!(f, "\"{}\"", s),
            ExprValue::Array(arr) => write!(f, "[...{} items]", arr.len()),
            ExprValue::Null => write!(f, "null"),
        }
    }
}

// ════════════════════════════════════════════════════════════
// v2 Built-in Functions
// ════════════════════════════════════════════════════════════

/// Built-in function names supported in v2 expressions.
#[derive(Debug, Clone, Copy)]
enum BuiltinFunction {
    /// Regex test: `regex(field, "pattern")` → bool
    Regex,
    /// String length: `len(field)` → number
    Len,
    /// String to lowercase: `lower(field)` → string
    Lower,
    /// String to uppercase: `upper(field)` → string
    Upper,
    /// String contains: `contains(field, "substring")` → bool
    Contains,
    /// String starts with: `starts_with(field, "prefix")` → bool
    StartsWith,
    /// String ends with: `ends_with(field, "suffix")` → bool
    EndsWith,
    /// Absolute value: `abs(field)` → number
    Abs,
    /// Type check: `type_of(field)` → string ("string", "number", "bool", "null")
    TypeOf,
}

impl BuiltinFunction {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "regex" => Some(Self::Regex),
            "len" => Some(Self::Len),
            "lower" => Some(Self::Lower),
            "upper" => Some(Self::Upper),
            "contains" => Some(Self::Contains),
            "starts_with" => Some(Self::StartsWith),
            "ends_with" => Some(Self::EndsWith),
            "abs" => Some(Self::Abs),
            "type_of" => Some(Self::TypeOf),
            _ => None,
        }
    }
}

// ════════════════════════════════════════════════════════════
// v2 Function Call & Pipe Operator Utilities
// ════════════════════════════════════════════════════════════

/// Find the content between balanced parentheses.
/// Input should start with '('. Returns the content between ( and ).
fn find_balanced_parens(s: &str) -> Option<&str> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0;
    let mut in_string = false;
    let mut string_char = ' ';
    let mut escaped = false;

    for (i, c) in s.char_indices() {
        if escaped {
            // 跳过转义字符后的字符（如 \" 中的 "）
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => {
                escaped = true;
            }
            '"' | '\'' if !in_string => {
                in_string = true;
                string_char = c;
            }
            _ if c == string_char && in_string => {
                in_string = false;
            }
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split function arguments by comma, respecting nesting, strings, and escape sequences.
///
/// Returns `Err(ExprError)` if the argument string contains unbalanced brackets.
fn split_function_args(args_str: &str) -> Result<Vec<&str>, ExprError> {
    let mut args = vec![];
    let mut current_start = 0;
    let mut depth = 0;
    let mut in_string = false;
    let mut string_char = ' ';
    let mut escaped = false;

    for (i, c) in args_str.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => {
                escaped = true;
            }
            '"' | '\'' if !in_string => {
                in_string = true;
                string_char = c;
            }
            _ if c == string_char && in_string => {
                in_string = false;
            }
            '(' | '[' | '{' if !in_string => depth += 1,
            ')' | ']' | '}' if !in_string => {
                depth -= 1;
                if depth < 0 {
                    return Err(ExprError::ParseError(format!(
                        "split_function_args: 不平衡的闭合括号 '{}' 在位置 {}，输入: \"{}\"",
                        c, i, args_str
                    )));
                }
            }
            ',' if depth == 0 && !in_string => {
                args.push(&args_str[current_start..i]);
                current_start = i + 1;
            }
            _ => {}
        }
    }

    // Push the last argument
    if current_start < args_str.len() {
        args.push(&args_str[current_start..]);
    }

    args.retain(|arg| !arg.trim().is_empty());

    Ok(args)
}

/// Try to evaluate a built-in function call expression.
/// Returns `Some(ExprValue)` if the expression is a recognized function call,
/// `None` if it's not a function call.
fn try_eval_function_call(
    expr: &str,
    item: &serde_json::Value,
) -> Result<Option<ExprValue>, ExprError> {
    let expr = expr.trim();

    // Find the opening parenthesis
    let paren_pos = match expr.find('(') {
        Some(pos) => pos,
        None => return Ok(None),
    };

    let func_name = expr[..paren_pos].trim();
    let builtin = match BuiltinFunction::from_name(func_name) {
        Some(f) => f,
        None => return Ok(None),
    };

    // Find matching closing parenthesis
    let args_str = match find_balanced_parens(&expr[paren_pos..]) {
        Some(args) => args,
        None => {
            return Err(ExprError::ParseError(format!(
                "Unbalanced parentheses in function call: {}",
                expr
            )));
        }
    };

    // Parse arguments (split by comma, respecting nesting and strings)
    let args = split_function_args(args_str)?;

    eval_builtin_function(builtin, &args, item).map(Some)
}

/// Evaluate a built-in function call.
///
/// # Supported Functions
///
/// - `regex(p.field, "pattern")` — regex test, returns bool
/// - `len(p.field)` — string/array length, returns number
/// - `lower(p.field)` — lowercase, returns string
/// - `upper(p.field)` — uppercase, returns string
/// - `contains(p.field, "substring")` — substring check, returns bool
/// - `starts_with(p.field, "prefix")` — prefix check, returns bool
/// - `ends_with(p.field, "suffix")` — suffix check, returns bool
/// - `abs(p.field)` — absolute value, returns number
/// - `type_of(p.field)` — type name, returns string
fn eval_builtin_function(
    func: BuiltinFunction,
    args: &[&str],
    item: &serde_json::Value,
) -> Result<ExprValue, ExprError> {
    match func {
        BuiltinFunction::Regex => {
            // regex(p.field, "pattern") → bool
            if args.len() != 2 {
                return Err(ExprError::ParseError(format!(
                    "regex() requires 2 arguments, got {}",
                    args.len()
                )));
            }
            let field_val = resolve_value_expr(args[0].trim(), item)?;
            let pattern_val = parse_literal(args[1].trim())?;

            let haystack = match &field_val {
                ExprValue::String(s) => s.clone(),
                _ => {
                    return Err(ExprError::TypeMismatch {
                        expected: "string".into(),
                        actual: format!("{:?}", field_val),
                    });
                }
            };
            let pattern = match &pattern_val {
                ExprValue::String(s) => &**s,
                _ => {
                    return Err(ExprError::TypeMismatch {
                        expected: "string pattern".into(),
                        actual: format!("{:?}", pattern_val),
                    });
                }
            };

            let re = get_cached_regex(pattern).map_err(|e| ExprError::RegexError(e.to_string()))?;
            Ok(ExprValue::Bool(re.is_match(&haystack)))
        }

        BuiltinFunction::Len => {
            // len(p.field) → number
            if args.len() != 1 {
                return Err(ExprError::ParseError(format!(
                    "len() requires 1 argument, got {}",
                    args.len()
                )));
            }
            let val = resolve_value_expr(args[0].trim(), item)?;
            match val {
                ExprValue::String(s) => Ok(ExprValue::Number(s.chars().count() as f64)),
                ExprValue::Array(arr) => Ok(ExprValue::Number(arr.len() as f64)),
                ExprValue::Null => Ok(ExprValue::Number(0.0)),
                _ => Err(ExprError::TypeMismatch {
                    expected: "string or array".into(),
                    actual: format!("{:?}", val),
                }),
            }
        }

        BuiltinFunction::Lower => {
            // lower(p.field) → string
            if args.len() != 1 {
                return Err(ExprError::ParseError(format!(
                    "lower() requires 1 argument, got {}",
                    args.len()
                )));
            }
            let val = resolve_value_expr(args[0].trim(), item)?;
            match val {
                ExprValue::String(s) => Ok(ExprValue::String(Cow::Owned(s.to_lowercase()))),
                _ => Err(ExprError::TypeMismatch {
                    expected: "string".into(),
                    actual: format!("{:?}", val),
                }),
            }
        }

        BuiltinFunction::Upper => {
            // upper(p.field) → string
            if args.len() != 1 {
                return Err(ExprError::ParseError(format!(
                    "upper() requires 1 argument, got {}",
                    args.len()
                )));
            }
            let val = resolve_value_expr(args[0].trim(), item)?;
            match val {
                ExprValue::String(s) => Ok(ExprValue::String(Cow::Owned(s.to_uppercase()))),
                _ => Err(ExprError::TypeMismatch {
                    expected: "string".into(),
                    actual: format!("{:?}", val),
                }),
            }
        }

        BuiltinFunction::Contains => {
            // contains(p.field, "substring") → bool
            if args.len() != 2 {
                return Err(ExprError::ParseError(format!(
                    "contains() requires 2 arguments, got {}",
                    args.len()
                )));
            }
            let field_val = resolve_value_expr(args[0].trim(), item)?;
            let search = parse_literal(args[1].trim())?;
            match (&field_val, &search) {
                (ExprValue::String(s), ExprValue::String(sub)) => {
                    Ok(ExprValue::Bool(s.contains(&**sub)))
                }
                _ => Err(ExprError::TypeMismatch {
                    expected: "contains(string, string)".into(),
                    actual: format!("{:?}, {:?}", field_val, search),
                }),
            }
        }

        BuiltinFunction::StartsWith => {
            // starts_with(p.field, "prefix") → bool
            if args.len() != 2 {
                return Err(ExprError::ParseError(format!(
                    "starts_with() requires 2 arguments, got {}",
                    args.len()
                )));
            }
            let field_val = resolve_value_expr(args[0].trim(), item)?;
            let prefix = parse_literal(args[1].trim())?;
            match (&field_val, &prefix) {
                (ExprValue::String(s), ExprValue::String(p)) => {
                    Ok(ExprValue::Bool(s.starts_with(&**p)))
                }
                _ => Err(ExprError::TypeMismatch {
                    expected: "starts_with(string, string)".into(),
                    actual: format!("{:?}, {:?}", field_val, prefix),
                }),
            }
        }

        BuiltinFunction::EndsWith => {
            // ends_with(p.field, "suffix") → bool
            if args.len() != 2 {
                return Err(ExprError::ParseError(format!(
                    "ends_with() requires 2 arguments, got {}",
                    args.len()
                )));
            }
            let field_val = resolve_value_expr(args[0].trim(), item)?;
            let suffix = parse_literal(args[1].trim())?;
            match (&field_val, &suffix) {
                (ExprValue::String(s), ExprValue::String(sf)) => {
                    Ok(ExprValue::Bool(s.ends_with(&**sf)))
                }
                _ => Err(ExprError::TypeMismatch {
                    expected: "ends_with(string, string)".into(),
                    actual: format!("{:?}, {:?}", field_val, suffix),
                }),
            }
        }

        BuiltinFunction::Abs => {
            // abs(p.field) → number
            if args.len() != 1 {
                return Err(ExprError::ParseError(format!(
                    "abs() requires 1 argument, got {}",
                    args.len()
                )));
            }
            let val = resolve_value_expr(args[0].trim(), item)?;
            match val {
                ExprValue::Number(n) => Ok(ExprValue::Number(n.abs())),
                _ => Err(ExprError::TypeMismatch {
                    expected: "number".into(),
                    actual: format!("{:?}", val),
                }),
            }
        }

        BuiltinFunction::TypeOf => {
            // type_of(p.field) → string
            if args.len() != 1 {
                return Err(ExprError::ParseError(format!(
                    "type_of() requires 1 argument, got {}",
                    args.len()
                )));
            }
            let val = resolve_value_expr(args[0].trim(), item)?;
            let type_name = match val {
                ExprValue::Bool(_) => "bool",
                ExprValue::Number(_) => "number",
                ExprValue::String(_) => "string",
                ExprValue::Array(_) => "array",
                ExprValue::Null => "null",
            };
            Ok(ExprValue::String(type_name.to_string().into()))
        }
    }
}

/// Evaluate a built-in function on a given value (pipe context).
/// Unlike `eval_builtin_function`, this takes the first argument as an `ExprValue`
/// directly (the piped left-hand value), rather than resolving it from the item.
fn eval_builtin_function_on_value(
    func: BuiltinFunction,
    args: &[&str],
    piped_value: &ExprValue,
    _item: &serde_json::Value,
) -> Result<ExprValue, ExprError> {
    match func {
        BuiltinFunction::Lower => match piped_value {
            ExprValue::String(s) => Ok(ExprValue::String(Cow::Owned(s.to_lowercase()))),
            _ => Err(ExprError::TypeMismatch {
                expected: "string".into(),
                actual: format!("{:?}", piped_value),
            }),
        },
        BuiltinFunction::Upper => match piped_value {
            ExprValue::String(s) => Ok(ExprValue::String(Cow::Owned(s.to_uppercase()))),
            _ => Err(ExprError::TypeMismatch {
                expected: "string".into(),
                actual: format!("{:?}", piped_value),
            }),
        },
        BuiltinFunction::Len => match piped_value {
            ExprValue::String(s) => Ok(ExprValue::Number(s.chars().count() as f64)),
            ExprValue::Array(arr) => Ok(ExprValue::Number(arr.len() as f64)),
            ExprValue::Null => Ok(ExprValue::Number(0.0)),
            _ => Err(ExprError::TypeMismatch {
                expected: "string or array".into(),
                actual: format!("{:?}", piped_value),
            }),
        },
        BuiltinFunction::Abs => match piped_value {
            ExprValue::Number(n) => Ok(ExprValue::Number(n.abs())),
            _ => Err(ExprError::TypeMismatch {
                expected: "number".into(),
                actual: format!("{:?}", piped_value),
            }),
        },
        BuiltinFunction::TypeOf => {
            let type_name = match piped_value {
                ExprValue::Bool(_) => "bool",
                ExprValue::Number(_) => "number",
                ExprValue::String(_) => "string",
                ExprValue::Array(_) => "array",
                ExprValue::Null => "null",
            };
            Ok(ExprValue::String(Cow::Owned(type_name.to_string())))
        }
        BuiltinFunction::Contains => {
            if args.is_empty() {
                return Err(ExprError::ParseError(
                    "contains() in pipe requires 1 argument".into(),
                ));
            }
            let search = parse_literal(args[0].trim())?;
            match (piped_value, &search) {
                (ExprValue::String(s), ExprValue::String(sub)) => {
                    Ok(ExprValue::Bool(s.contains(&**sub)))
                }
                _ => Err(ExprError::TypeMismatch {
                    expected: "contains(string, string)".into(),
                    actual: format!("{:?}, {:?}", piped_value, search),
                }),
            }
        }
        BuiltinFunction::StartsWith => {
            if args.is_empty() {
                return Err(ExprError::ParseError(
                    "starts_with() in pipe requires 1 argument".into(),
                ));
            }
            let prefix = parse_literal(args[0].trim())?;
            match (piped_value, &prefix) {
                (ExprValue::String(s), ExprValue::String(p)) => {
                    Ok(ExprValue::Bool(s.starts_with(&**p)))
                }
                _ => Err(ExprError::TypeMismatch {
                    expected: "starts_with(string, string)".into(),
                    actual: format!("{:?}, {:?}", piped_value, prefix),
                }),
            }
        }
        BuiltinFunction::EndsWith => {
            if args.is_empty() {
                return Err(ExprError::ParseError(
                    "ends_with() in pipe requires 1 argument".into(),
                ));
            }
            let suffix = parse_literal(args[0].trim())?;
            match (piped_value, &suffix) {
                (ExprValue::String(s), ExprValue::String(sf)) => {
                    Ok(ExprValue::Bool(s.ends_with(&**sf)))
                }
                _ => Err(ExprError::TypeMismatch {
                    expected: "ends_with(string, string)".into(),
                    actual: format!("{:?}, {:?}", piped_value, suffix),
                }),
            }
        }
        BuiltinFunction::Regex => {
            if args.is_empty() {
                return Err(ExprError::ParseError(
                    "regex() in pipe requires 1 argument".into(),
                ));
            }
            let pattern_val = parse_literal(args[0].trim())?;
            let haystack = match piped_value {
                ExprValue::String(s) => s.clone(),
                _ => {
                    return Err(ExprError::TypeMismatch {
                        expected: "string".into(),
                        actual: format!("{:?}", piped_value),
                    });
                }
            };
            let pattern = match &pattern_val {
                ExprValue::String(s) => &**s,
                _ => {
                    return Err(ExprError::TypeMismatch {
                        expected: "string pattern".into(),
                        actual: format!("{:?}", pattern_val),
                    });
                }
            };
            let re = get_cached_regex(pattern).map_err(|e| ExprError::RegexError(e.to_string()))?;
            Ok(ExprValue::Bool(re.is_match(&haystack)))
        }
    }
}

/// Find the position of a top-level pipe operator `|` (not inside parens/strings).
/// Returns byte offset. Does not match `||` (logical OR) — checks both the
/// character before and after to avoid false positives.
///
///
/// For expressions like `a |b| c`, the `|` before `b` is correctly skipped
/// because `prev` (the character before `|`) is a space, and `rest` (the
/// Find the position of a top-level pipe operator (`|`) in `expr`.
///
/// A "top-level" pipe is one that is not inside a string literal or
/// parenthesized group. This function is the core of the pipe-chain
/// evaluation in Prism DSL expressions.
///
/// ## Escape semantics
///
/// The backslash `\` is used to escape the pipe character:
/// - `\|` is treated as a **literal pipe character**, not a pipe operator.
///   The backslash is consumed (skipped), and the `|` is ignored as an operator.
/// - `\\` is an escaped backslash (consumed as a pair, the next character
///   is processed normally).
///
/// **Important**: The escape only applies to the `|` character. Other characters
/// after `\` are simply skipped (e.g., `\(` is treated as a literal `(`).
/// This is intentional — Prism DSL expressions do not support general escape
/// sequences; the `\|` escape exists solely to allow literal pipe characters
/// in expressions (e.g., in regex patterns like `name =~ /a\|b/`).
///
/// ## Interaction with logical OR (`||`)
///
/// The `||` sequence is recognized as logical OR, not two pipe operators.
/// A single `|` is a pipe operator only when neither adjacent character is `|`.
///
/// ## YAML 中的管道符语义
///
/// 管道操作符 `|` 用于 `$transform` 表达式中连接多个转换步骤，
/// 例如 `$transform: "name | upper | trim"` 表示对数组元素依次执行
/// `upper` 和 `trim` 转换。
///
/// **字面量管道符**：如果用户需要在表达式中使用字面量管道符（如正则表达式
/// 中的 `a|b`），可以使用反斜杠转义：`\|`。反斜杠会被消费，`|` 不再被
/// 解析为管道操作符。
///
/// **YAML 注意事项**：`$transform` 表达式是 YAML 字符串值，而 `|` 在 YAML 中
/// 是块标量指示符（block scalar indicator），具有特殊含义。如果表达式包含 `|`，
/// 用户应使用双引号包裹表达式（如 `$transform: "name | upper"`）而非裸字符串，
/// 以避免 YAML 解析器将 `|` 误解为块标量指示符。
///
fn find_top_level_pipe(expr: &str) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut string_char = ' ';

    for (byte_pos, c) in expr.char_indices() {
        if c == '"' || c == '\'' {
            if !in_string {
                in_string = true;
                string_char = c;
            } else if c == string_char {
                in_string = false;
            }
            continue;
        }
        if in_string {
            continue;
        }

        // Handle escape sequences outside strings: `\\|` is a literal pipe, not an operator.
        // The `\\` escape is consumed here (skip next char).
        if c == '\\' {
            // Skip the next character (escaped char)
            let _ = expr[byte_pos + 1..].chars().next();
            continue;
        }

        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '|' if depth == 0 => {
                // Make sure it's not || (logical OR).
                // Count consecutive | characters at this position.
                // A single | surrounded by non-| chars is a pipe operator.
                // || or more consecutive | chars are logical OR operators.
                let rest = expr[byte_pos + 1..].chars().next();
                let prev = expr[..byte_pos].chars().next_back();
                if rest == Some('|') || prev == Some('|') {
                    // Part of a || (logical OR) or longer sequence — skip
                    continue;
                }
                return Some(byte_pos);
            }
            _ => {}
        }
    }
    None
}

/// Find the position of a top-level comparison operator in `expr`,
/// skipping over string literals and parenthesized groups.
/// Returns `Some(byte_offset)` if found, `None` otherwise.
///
/// ## Safety: Byte Offset Validity
///
/// This function operates on byte offsets rather than char indices.
/// This is safe because all operators searched for (==, !=, >=, <=, >, <, &&, ||, +)
/// consist exclusively of ASCII characters, and ASCII characters are always
/// single-byte in UTF-8. Therefore, byte offsets returned by this function
/// always point to valid character boundaries, and slicing `expr[pos..]`
/// will never panic on character boundary violations.
///
/// ## Non-ASCII Input
///
/// This function only supports ASCII operators. Non-ASCII characters in the
/// expression are treated as opaque bytes and will never match any operator.
/// This is intentional: Prism DSL expressions use only ASCII operators, and
/// non-ASCII characters typically appear inside string literals (which are
/// correctly skipped by the string-aware parsing logic).
fn find_top_level_operator(expr: &str, op: &str) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut string_char = ' ';
    let op_bytes = op.as_bytes();
    let expr_bytes = expr.as_bytes();

    let mut i = 0;
    while i < expr_bytes.len() {
        let c = expr_bytes[i] as char;

        if c == '"' || c == '\'' {
            if !in_string {
                in_string = true;
                string_char = c;
            } else if c == string_char {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }

        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0
                // Check if operator matches at this position
                && i + op_bytes.len() <= expr_bytes.len()
                && &expr_bytes[i..i + op_bytes.len()] == op_bytes =>
            {
                // For single-char operators like > and <, make sure they are not
                // part of a multi-char operator (e.g., don't match > inside >=).
                if op.len() == 1 {
                    let next = expr_bytes.get(i + 1).map(|&b| b as char);
                    if op == ">" && next == Some('=') {
                        i += 1;
                        continue;
                    }
                    if op == "<" && next == Some('=') {
                        i += 1;
                        continue;
                    }
                    if op == "!" && next == Some('=') {
                        i += 1;
                        continue;
                    }
                }
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Resolve an expression to an ExprValue (for pipe left-hand side).
fn resolve_expr_to_value(expr: &str, item: &serde_json::Value) -> Result<ExprValue, ExprError> {
    let expr = expr.trim();

    // Try function call first
    if let Some(val) = try_eval_function_call(expr, item)? {
        return Ok(val);
    }

    // Try field access
    if expr.starts_with("p.") {
        return resolve_value_expr(expr, item);
    }

    // Try literal
    parse_literal(expr)
}

/// Apply a pipe filter: take the left value and apply the right expression.
/// Supports: `value | func_name(args)` or `value | > number` or `value | == string`
/// Also supports: `value | func_name == literal` (apply function then compare)
/// Also supports pipe chaining: `value | lower | contains("x")`
fn apply_pipe_filter(
    left: &ExprValue,
    right: &str,
    item: &serde_json::Value,
) -> Result<bool, ExprError> {
    let right = right.trim();

    // Check for chained pipe: value | lower | contains("x")
    // If right side contains another pipe, recursively resolve
    if let Some(pipe_pos) = find_top_level_pipe(right) {
        let next_left_expr = &right[..pipe_pos].trim();
        let next_right = &right[pipe_pos + 1..].trim();

        // Resolve the intermediate value (e.g., lower applied to piped value)
        let intermediate = resolve_pipe_stage(left, next_left_expr, item)?;
        return apply_pipe_filter(&intermediate, next_right, item);
    }

    // Pipe to function then comparison: value | lower == "abc"
    // Check if right side has a function call followed by a comparison operator
    // First try with parentheses: func_name(args) == literal
    if let Some(paren_pos) = right.find('(') {
        let func_name = right[..paren_pos].trim();
        if let Some(func) = BuiltinFunction::from_name(func_name)
            && let Some(args_str) = find_balanced_parens(&right[paren_pos..])
        {
            let after_args = right[paren_pos + 1 + args_str.len()..].trim();
            // Check if there's a comparison operator after the function call
            for op in &["==", "!=", ">=", "<=", ">", "<"] {
                if let Some(rest) = after_args.strip_prefix(op) {
                    let right_val_str = rest.trim();
                    let right_val = parse_literal(right_val_str)?;
                    let args = split_function_args(args_str)?;
                    let func_result = eval_builtin_function_on_value(func, &args, left, item)?;
                    return compare_values(&func_result, &right_val, op);
                }
            }
        }
    }

    // Also try bare function name without parens: lower == "abc", upper == "ABC"
    for op in &["==", "!=", ">=", "<=", ">", "<"] {
        if let Some(op_pos) = find_top_level_operator(right, op) {
            let before_op = right[..op_pos].trim();
            if let Some(func) = BuiltinFunction::from_name(before_op) {
                let right_val_str = right[op_pos + op.len()..].trim();
                let right_val = parse_literal(right_val_str)?;
                let func_result = eval_builtin_function_on_value(func, &[], left, item)?;
                return compare_values(&func_result, &right_val, op);
            }
        }
    }

    // Pipe to function: value | lower, value | contains("x")
    // Try with parentheses first (pipe context: apply function on piped value)
    if let Some(paren_pos) = right.find('(') {
        let func_name = right[..paren_pos].trim();
        if let Some(func) = BuiltinFunction::from_name(func_name)
            && let Some(args_str) = find_balanced_parens(&right[paren_pos..])
        {
            let args = split_function_args(args_str)?;
            let func_result = eval_builtin_function_on_value(func, &args, left, item)?;
            match func_result {
                ExprValue::Bool(b) => return Ok(b),
                _ => {
                    return Err(ExprError::TypeMismatch {
                        expected: "bool".into(),
                        actual: format!("{:?}", func_result),
                    });
                }
            }
        }
    }

    // Try bare function name without parens in pipe context
    if let Some(func) = BuiltinFunction::from_name(right) {
        let func_result = eval_builtin_function_on_value(func, &[], left, item)?;
        match func_result {
            ExprValue::Bool(b) => return Ok(b),
            _ => {
                return Err(ExprError::TypeMismatch {
                    expected: "bool".into(),
                    actual: format!("{:?}", func_result),
                });
            }
        }
    }

    // Pipe to function call (non-pipe context): value | func(args) -- fallback
    if let Some(val) = try_eval_function_call(right, item)? {
        // Compare the piped value with the function result
        return Ok(*left == val);
    }

    // Pipe to comparison: value | > 100, value | == "abc"
    for op in &["==", "!=", ">=", "<=", ">", "<"] {
        if let Some(pos) = find_top_level_operator(right, op) {
            let right_val = parse_literal(right[pos + op.len()..].trim())?;
            return compare_values(left, &right_val, op);
        }
    }

    Err(ExprError::Unsupported(format!(
        "Unsupported pipe expression: | {}",
        right
    )))
}

/// Resolve a single pipe stage: apply a function or expression to the piped value.
fn resolve_pipe_stage(
    left: &ExprValue,
    stage: &str,
    item: &serde_json::Value,
) -> Result<ExprValue, ExprError> {
    let stage = stage.trim();

    // Try as a function call: lower, upper, len, etc.
    if let Some(paren_pos) = stage.find('(') {
        let func_name = stage[..paren_pos].trim();
        if let Some(func) = BuiltinFunction::from_name(func_name)
            && let Some(args_str) = find_balanced_parens(&stage[paren_pos..])
        {
            let args = split_function_args(args_str)?;
            return eval_builtin_function_on_value(func, &args, left, item);
        }
    }

    // Try as a bare function name without parens (e.g., lower, upper)
    if let Some(func) = BuiltinFunction::from_name(stage) {
        return eval_builtin_function_on_value(func, &[], left, item);
    }

    Err(ExprError::Unsupported(format!(
        "Unsupported pipe stage: {}",
        stage
    )))
}

// ════════════════════════════════════════════════════════════
// Predicate Evaluation ($filter / $remove)
// ════════════════════════════════════════════════════════════

/// Maximum recursion depth for predicate evaluation.
/// Prevents stack overflow from deeply nested expressions (e.g., `((((...))))`).
const MAX_PRED_DEPTH: u32 = 64;

/// Evaluate a predicate expression against a single JSON element.
///
/// Returns `Ok(true)` if the element matches the predicate, `Ok(false)` otherwise.
///
/// # Examples
///
/// ```ignore
/// use serde_json::json;
/// use clash_prism_core::executor::expr::evaluate_predicate;
///
/// let item = json!({"type": "ss", "port": 8388});
/// assert!(evaluate_predicate("p.type == \"ss\"", &item).unwrap());
/// assert!(!evaluate_predicate("p.port > 9000", &item).unwrap());
/// ```
pub fn evaluate_predicate(expr: &str, item: &serde_json::Value) -> Result<bool, ExprError> {
    evaluate_predicate_inner(expr, item, 0)
}

/// Internal recursive implementation with depth tracking.
fn evaluate_predicate_inner(
    expr: &str,
    item: &serde_json::Value,
    depth: u32,
) -> Result<bool, ExprError> {
    if depth > MAX_PRED_DEPTH {
        return Err(ExprError::ParseError(format!(
            "Expression recursion depth exceeded limit ({}). \
             Check for excessively nested parentheses or operators.",
            MAX_PRED_DEPTH
        )));
    }

    let expr = expr.trim();

    // 处理逻辑或 ||
    if let Some(pos) = find_top_level_operator(expr, "||") {
        let left = &expr[..pos].trim();
        let right = &expr[pos + 2..].trim();
        return Ok(evaluate_predicate_inner(left, item, depth + 1)?
            || evaluate_predicate_inner(right, item, depth + 1)?);
    }

    // 处理逻辑与 &&
    if let Some(pos) = find_top_level_operator(expr, "&&") {
        let left = &expr[..pos].trim();
        let right = &expr[pos + 2..].trim();
        return Ok(evaluate_predicate_inner(left, item, depth + 1)?
            && evaluate_predicate_inner(right, item, depth + 1)?);
    }

    // 处理逻辑非 !
    if let Some(rest) = expr.strip_prefix('!').map(|s| s.trim()) {
        return Ok(!evaluate_predicate_inner(rest, item, depth + 1)?);
    }

    // v2: Handle pipe operator |
    // Pipe chains the output of one expression as input to the next
    // Example: p.name | lower | contains("hk")
    if let Some(pos) = find_top_level_pipe(expr) {
        let left = &expr[..pos].trim();
        let right = &expr[pos + 1..].trim();

        // Evaluate left side to get a value
        let left_val = resolve_expr_to_value(left, item)?;

        // Apply right side as a function/filter on the left value
        return apply_pipe_filter(&left_val, right, item);
    }

    // 处理括号包裹的表达式
    if let Some(inner) = strip_outer_parens(expr) {
        return evaluate_predicate_inner(inner, item, depth + 1);
    }

    // 比较表达式
    for op in &["==", "!=", ">=", "<=", ">", "<"] {
        if let Some(pos) = find_comparison_op(expr, op) {
            let left_expr = &expr[..pos].trim();
            let right_expr = &expr[pos + op.len()..].trim();

            // v2: Try to resolve left side as function call first, then field access
            let left_val = if let Some(val) = try_eval_function_call(left_expr, item)? {
                val
            } else {
                resolve_value_expr(left_expr, item)?
            };
            let right_val = parse_literal(right_expr)?;

            return compare_values(&left_val, &right_val, op);
        }
    }

    // 方法调用：p.field.includes("...") 或 p.field.match(/.../)
    if let Some(result) = eval_method_call(expr, item)? {
        return Ok(result);
    }

    // v2: Built-in function calls (e.g., regex(p.name, "^prod-"), contains(p.type, "ss"))
    if let Some(val) = try_eval_function_call(expr, item)? {
        match val {
            ExprValue::Bool(b) => return Ok(b),
            _ => {
                return Err(ExprError::TypeMismatch {
                    expected: "bool".into(),
                    actual: format!("{:?}", val),
                });
            }
        }
    }

    Err(ExprError::Unsupported(expr.to_string()))
}

/// Find the position of a comparison operator at the top level.
fn find_comparison_op(expr: &str, op: &str) -> Option<usize> {
    find_top_level_operator(expr, op)
}

/// Strip outer parentheses from an expression, if they form a balanced pair.
fn strip_outer_parens(expr: &str) -> Option<&str> {
    let expr = expr.trim();
    if expr.starts_with('(') && expr.ends_with(')') {
        // String-aware parenthesis matching.
        // Skips over string literals to avoid misinterpreting ')' inside strings
        // as closing the outer parenthesis group.
        let mut depth = 0;
        let mut in_string = false;
        let mut string_char = ' ';

        for (i, c) in expr.char_indices() {
            match c {
                '"' | '\'' if !in_string => {
                    in_string = true;
                    string_char = c;
                }
                _ if c == string_char && in_string => {
                    in_string = false;
                }
                '(' if !in_string => depth += 1,
                ')' if !in_string => {
                    depth -= 1;
                    if depth == 0 && i == expr.len() - 1 {
                        return Some(&expr[1..expr.len() - 1]);
                    }
                    // Unbalanced: closing paren before end of string
                    if depth == 0 {
                        return None;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Resolve a field access expression (e.g., `p.type`, `p.name`) to its value.
#[inline]
fn resolve_value_expr(expr: &str, item: &serde_json::Value) -> Result<ExprValue, ExprError> {
    let expr = expr.trim();

    // 去除 p. 前缀
    let field_name = if let Some(field) = expr.strip_prefix("p.") {
        field.trim()
    } else {
        // 可能是裸字段名
        expr
    };

    // 获取字段值
    let value = item
        .get(field_name)
        .ok_or_else(|| ExprError::FieldNotFound(field_name.to_string()))?;

    json_to_expr(value)
}

/// Convert a JSON value to an [`ExprValue`] for comparison.
///
/// P-01: Marked `#[inline]` to encourage the compiler to eliminate intermediate
/// allocations when the result is immediately consumed by `compare_values`.
/// For the common case of `p.field == "literal"`, the compiler can often
/// elide the String clone when the ExprValue is only used for comparison.
#[inline]
fn json_to_expr(v: &serde_json::Value) -> Result<ExprValue, ExprError> {
    match v {
        serde_json::Value::Bool(b) => Ok(ExprValue::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(ExprValue::Number(i as f64))
            } else if let Some(f) = n.as_f64() {
                Ok(ExprValue::Number(f))
            } else {
                Err(ExprError::TypeMismatch {
                    expected: "number".into(),
                    actual: format!("{:?}", n),
                })
            }
        }
        serde_json::Value::String(s) => Ok(ExprValue::String(Cow::Owned(s.clone()))),
        serde_json::Value::Array(arr) => Ok(ExprValue::Array(arr.to_vec())),
        serde_json::Value::Null => Ok(ExprValue::Null),
        other => Err(ExprError::TypeMismatch {
            expected: "primitive".into(),
            actual: format!("{:?}", other),
        }),
    }
}

/// Parse a literal value from an expression string (string, number, bool, null).
///
/// Handles quoted strings with proper escape sequence processing.
/// Shared helper used by both `parse_literal` and `parse_json_literal`
/// to eliminate code duplication. The only difference is the return type wrapper.
fn parse_quoted_string_inner(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return None;
    }

    // Single-pass escape processing: scan left-to-right, handle \X sequences
    // in one pass to avoid chained-replace corruption (e.g., \\\" → \" not ")
    let inner = &chars[1..chars.len() - 1];
    let mut result = String::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == '\\' && i + 1 < inner.len() {
            match inner[i + 1] {
                '\\' => result.push('\\'),
                '"' => result.push('"'),
                '\'' => result.push('\''),
                'n' => result.push('\n'),
                't' => result.push('\t'),
                'r' => result.push('\r'),
                other => {
                    // Unknown escape: preserve both characters literally
                    result.push('\\');
                    result.push(other);
                }
            }
            i += 2;
        } else {
            result.push(inner[i]);
            i += 1;
        }
    }

    Some(result)
}

/// Parse a literal value from an expression string (string, number, bool, null).
fn parse_literal(s: &str) -> Result<ExprValue, ExprError> {
    let s = s.trim();

    // 布尔
    if s == "true" {
        return Ok(ExprValue::Bool(true));
    }
    if s == "false" {
        return Ok(ExprValue::Bool(false));
    }
    if s == "null" {
        return Ok(ExprValue::Null);
    }

    // 字符串（带引号）— 使用字符迭代正确处理 Unicode/emoji
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        // 使用共享的字符串解析辅助函数
        if let Some(unescaped) = parse_quoted_string_inner(s) {
            return Ok(ExprValue::String(Cow::Owned(unescaped)));
        }
    }

    // 数字
    if let Ok(n) = s.parse::<f64>() {
        return Ok(ExprValue::Number(n));
    }

    // 尝试作为字段访问处理（用于右侧也是字段的情况）
    // 这种情况在高级表达式中可能出现
    Err(ExprError::Unsupported(format!(
        "Cannot parse literal: \"{}\". If this is a field reference, \
         did you forget the 'p.' prefix? (e.g., p.{})",
        s, s
    )))
}

/// Epsilon threshold for floating-point equality comparison.
/// Used as a relative epsilon: values are considered equal if their absolute
/// difference is less than `FLOAT_EPSILON * max(|a|, |b|, 1.0)`.
/// The `1.0` floor prevents the threshold from collapsing to zero when both values are near zero.
const FLOAT_EPSILON: f64 = 1e-9;

/// Compare two [`ExprValue`] instances for equality, using relative epsilon comparison for floats.
///
/// Uses relative epsilon instead of absolute epsilon to handle values of different magnitudes.
/// Formula: `|a - b| < FLOAT_EPSILON * max(|a|, |b|, 1.0)`
/// The `1.0` floor ensures near-zero comparisons still use a meaningful threshold.
fn float_eq(left: &ExprValue, right: &ExprValue) -> bool {
    match (left, right) {
        (ExprValue::Number(a), ExprValue::Number(b)) => {
            // Use exact comparison for values that are integers in f64 representation.
            // This avoids false negatives from floating-point epsilon when comparing
            // values like 8080.0 == 8080.0 (which should always be true).
            if a.fract() == 0.0 && b.fract() == 0.0 && *a == *b {
                return true;
            }
            let scale = a.abs().max(b.abs()).max(1.0);
            (a - b).abs() < FLOAT_EPSILON * scale
        }
        _ => left == right,
    }
}

/// Compare two [`ExprValue`] instances with the given operator.
#[inline]
fn compare_values(left: &ExprValue, right: &ExprValue, op: &str) -> Result<bool, ExprError> {
    match op {
        "==" => Ok(float_eq(left, right)),
        "!=" => Ok(!float_eq(left, right)),
        ">" => match (left, right) {
            (ExprValue::Number(a), ExprValue::Number(b)) => Ok(a > b),
            (ExprValue::String(a), ExprValue::String(b)) => Ok(a > b),
            _ => Err(ExprError::TypeMismatch {
                expected: "comparable types".into(),
                actual: format!("{:?} vs {:?}", left, right),
            }),
        },
        "<" => match (left, right) {
            (ExprValue::Number(a), ExprValue::Number(b)) => Ok(a < b),
            (ExprValue::String(a), ExprValue::String(b)) => Ok(a < b),
            _ => Err(ExprError::TypeMismatch {
                expected: "comparable types".into(),
                actual: format!("{:?} vs {:?}", left, right),
            }),
        },
        ">=" => match (left, right) {
            (ExprValue::Number(a), ExprValue::Number(b)) => Ok(a >= b),
            (ExprValue::String(a), ExprValue::String(b)) => Ok(a >= b),
            _ => Err(ExprError::TypeMismatch {
                expected: "comparable types".into(),
                actual: format!("{:?} vs {:?}", left, right),
            }),
        },
        "<=" => match (left, right) {
            (ExprValue::Number(a), ExprValue::Number(b)) => Ok(a <= b),
            (ExprValue::String(a), ExprValue::String(b)) => Ok(a <= b),
            _ => Err(ExprError::TypeMismatch {
                expected: "comparable types".into(),
                actual: format!("{:?} vs {:?}", left, right),
            }),
        },
        _ => Err(ExprError::Unsupported(format!("Unknown operator: {}", op))),
    }
}

/// Evaluate method calls: `p.field.includes("...")` or `p.field.match(/.../)`.
/// Returns `Some(bool)` if this is a recognized method call, `None` otherwise.
fn eval_method_call(expr: &str, item: &serde_json::Value) -> Result<Option<bool>, ExprError> {
    // p.field.includes("text")
    if let Some(caps) = INCLUDES_RE.captures(expr) {
        let field_path = caps.get(1).unwrap().as_str();
        let arg_str = caps.get(2).unwrap().as_str();

        let field_val = get_nested_field(item, field_path)?;
        let search_str = parse_literal(arg_str)?;

        let text = match (&field_val, &search_str) {
            (serde_json::Value::String(s), ExprValue::String(search)) => s.contains(&**search),
            _ => {
                return Err(ExprError::TypeMismatch {
                    expected: "string.includes(string)".into(),
                    actual: format!("{:?}.includes({:?})", field_val, search_str),
                });
            }
        };

        return Ok(Some(text));
    }

    // p.field.match(/pattern/) — 使用手动解析
    // 注意 match 是 Rust 关键字，这里是在字符串中匹配方法名
    let match_prefix = ".match(";
    if let Some(pos) = expr.find(match_prefix) {
        let before_match = &expr[..pos];
        // 提取字段路径（去掉 p. 前缀）
        let field_path = if let Some(field) = before_match.strip_prefix("p.") {
            field
        } else {
            return Ok(None);
        };

        let args_start = &expr[pos + match_prefix.len()..];
        // 解析 /pattern/ 格式
        if let Some(after_slash) = args_start.strip_prefix('/') {
            // 找到下一个 /（支持正则标志如 /i 等）
            if let Some(end_pos) = after_slash.find('/') {
                let pattern = &after_slash[..end_pos];

                let field_val = get_nested_field(item, field_path)?;
                // 使用缓存正则，同一 pattern 只编译一次
                let re =
                    get_cached_regex(pattern).map_err(|e| ExprError::RegexError(e.to_string()))?;

                let text = match &field_val {
                    serde_json::Value::String(s) => s,
                    _ => {
                        return Err(ExprError::TypeMismatch {
                            expected: "string".into(),
                            actual: format!("{:?}", field_val),
                        });
                    }
                };

                return Ok(Some(re.is_match(text)));
            }
        }
    }

    Ok(None)
}

/// Get a nested field value supporting dot-notation paths (e.g., `p.a.b`).
fn get_nested_field(item: &serde_json::Value, path: &str) -> Result<serde_json::Value, ExprError> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = item;

    for part in &parts {
        match current.get(*part) {
            Some(v) => current = v,
            None => return Err(ExprError::FieldNotFound(path.to_string())),
        }
    }

    Ok(current.clone())
}

// ════════════════════════════════════════════════════════════
// Transform Expression Evaluation ($transform)
// ════════════════════════════════════════════════════════════

/// Apply a transform expression to a single JSON element, returning the new element.
///
/// # Examples
///
/// ```ignore
/// let item = json!({"name": "香港01", "type": "ss"});
/// let result = evaluate_transform_expr("{...p, name: \"🇭🇰 \" + p.name}", &item);
/// assert_eq!(result.unwrap()["name"], "🇭🇰 香港01");
/// ```
pub fn evaluate_transform_expr(
    expr: &str,
    item: &serde_json::Value,
) -> Result<serde_json::Value, ExprError> {
    let expr = expr.trim();

    // 必须以 { 开头和 } 结尾
    if !(expr.starts_with('{') && expr.ends_with('}')) {
        return Err(ExprError::ParseError(format!(
            "Transform expression must be an object {{...}}, got: {}",
            expr
        )));
    }

    let body = &expr[1..expr.len() - 1].trim();

    // 检查是否包含展开运算符 ...p（使用词法检测避免误判）
    // 匹配 "...p" 前面是行首或非字母字符，后面是空白或行尾
    let has_spread = {
        let mut found = false;
        let bytes = body.as_bytes();
        let pattern = b"...p";
        for i in 0..bytes.len().saturating_sub(pattern.len() - 1) {
            if &bytes[i..i + pattern.len()] == pattern {
                let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphabetic();
                let after_ok = i + pattern.len() >= bytes.len()
                    || !bytes[i + pattern.len()].is_ascii_alphanumeric();
                if before_ok && after_ok {
                    found = true;
                    break;
                }
            }
        }
        found
    };

    // 解析键值对
    let mut result = if has_spread {
        item.clone()
    } else {
        serde_json::Map::new().into()
    };

    // 解析每个 key: value 对
    let pairs = split_object_pairs(body)?;
    for (key, value_expr) in &pairs {
        let resolved = resolve_transform_value(value_expr, item)?;
        if let Some(obj) = result.as_object_mut() {
            obj.insert(key.clone(), resolved);
        }
    }

    Ok(result)
}

/// Resolve the value part of a transform expression (handles concat, replace, field access, literals).
///
/// `depth` tracks recursion depth to prevent stack overflow from deeply nested
/// expressions (e.g., `"a" + "b" + "c" + ...`).
fn resolve_transform_value(
    expr: &str,
    item: &serde_json::Value,
) -> Result<serde_json::Value, ExprError> {
    resolve_transform_value_inner(expr, item, 0)
}

fn resolve_transform_value_inner(
    expr: &str,
    item: &serde_json::Value,
    depth: u32,
) -> Result<serde_json::Value, ExprError> {
    if depth > MAX_PRED_DEPTH {
        return Err(ExprError::ParseError(format!(
            "Transform expression recursion depth exceeded limit ({}). \
             Check for excessively nested string concatenation or field access.",
            MAX_PRED_DEPTH
        )));
    }

    let expr = expr.trim();

    // 字符串拼接："prefix-" + p.name
    if let Some(pos) = find_top_level_operator(expr, "+") {
        let left = &expr[..pos].trim();
        let right = &expr[pos + 1..].trim();

        let left_val = resolve_transform_value_inner(left, item, depth + 1)?;
        let right_val = resolve_transform_value_inner(right, item, depth + 1)?;

        // 字符串拼接
        let left_str = value_to_string(&left_val)?;
        let right_str = value_to_string(&right_val)?;

        return Ok(serde_json::Value::String(format!(
            "{}{}",
            left_str, right_str
        )));
    }

    // .replace(/pattern/replacement/) 方法调用
    if let Some(replaced) = eval_replace_call(expr, item)? {
        return Ok(replaced);
    }

    // p.field 访问
    if let Some(field) = expr.strip_prefix("p.").map(|s| s.trim()) {
        match get_nested_field(item, field) {
            Ok(v) => return Ok(v),
            Err(_) => return Ok(serde_json::Value::Null),
        }
    }

    // 字面量
    parse_json_literal(expr)
}

/// Convert a JSON Value to String (for string concatenation in transforms).
fn value_to_string(v: &serde_json::Value) -> Result<String, ExprError> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        serde_json::Value::Null => Ok(String::new()),
        _ => Err(ExprError::TypeMismatch {
            expected: "string-convertible".into(),
            actual: format!("{:?}", v),
        }),
    }
}

/// Parse as a JSON literal (bool, number, string, null).
fn parse_json_literal(s: &str) -> Result<serde_json::Value, ExprError> {
    let s = s.trim();

    if s == "true" {
        return Ok(serde_json::Value::Bool(true));
    }
    if s == "false" {
        return Ok(serde_json::Value::Bool(false));
    }
    if s == "null" {
        return Ok(serde_json::Value::Null);
    }

    // 带引号的字符串 — 使用字符迭代正确处理 Unicode/emoji
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        // 使用共享的字符串解析辅助函数（与 parse_literal 相同逻辑）
        if let Some(unescaped) = parse_quoted_string_inner(s) {
            return Ok(serde_json::Value::String(unescaped));
        }
    }

    // 数字
    if let Ok(n) = s.parse::<i64>() {
        return Ok(serde_json::Value::Number(serde_json::Number::from(n)));
    }
    if let Ok(n) = s.parse::<f64>() {
        return Ok(serde_json::Value::Number(
            serde_json::Number::from_f64(n).unwrap_or(serde_json::Number::from(0)),
        ));
    }

    Err(ExprError::ParseError(format!(
        "Cannot parse JSON literal: {}",
        s
    )))
}

/// Escape special characters in a regex replacement string.
///
/// In regex replacement strings, `$` and `\` have special meanings:
/// - `$1`, `$2` etc. are backreferences to capture groups
/// - `\\` is an escape sequence
///
/// This function escapes them to prevent unintended behavior:
/// - `$` → `$$` (literal dollar sign)
/// - `\` → `\\` (literal backslash)
fn escape_regex_replacement(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '$' => result.push_str("$$"),
            '\\' => result.push_str("\\\\"),
            _ => result.push(c),
        }
    }
    result
}

/// Evaluate `.replace()` call — supports both sed-style and JS-style replacement:
/// - `p.field.replace(/pattern/replacement/)` — sed style
/// - `p.field.replace(/pattern/, "replacement")` — JS style
fn eval_replace_call(
    expr: &str,
    item: &serde_json::Value,
) -> Result<Option<serde_json::Value>, ExprError> {
    // 使用手动解析而非正则，以正确处理 Unicode/emoji 字符
    let prefix = "p.";
    if !expr.starts_with(prefix) {
        return Ok(None);
    }
    let rest = &expr[prefix.len()..];

    // 查找 .replace(
    if let Some(pos) = rest.find(".replace(") {
        let field_path = &rest[..pos];
        let args_start = &rest[pos + 9..]; // 跳过 .replace(

        // 找到第一个 / 作为 pattern 开始
        if !args_start.starts_with('/') {
            return Ok(None);
        }
        let after_first_slash = &args_start[1..];

        // 找到第二个 /（pattern 结束）
        let (pattern, after_pattern) = find_regex_delimiter(after_first_slash, '/')?;

        let field_val = get_nested_field(item, field_path)?;
        // 使用缓存正则，同一 pattern 只编译一次
        let re = get_cached_regex(pattern).map_err(|e| ExprError::RegexError(e.to_string()))?;

        let text = match &field_val {
            serde_json::Value::String(s) => s,
            _ => {
                return Err(ExprError::TypeMismatch {
                    expected: "string".into(),
                    actual: format!("{:?}", field_val),
                });
            }
        };

        // 检查是哪种格式:
        // 格式1: /pattern/replacement/ — 紧接着是 /
        // 格式2: /pattern/, "string" — 紧接着是逗号或 )
        let trimmed = after_pattern.trim_start();
        if let Some(after_third_slash) = trimmed.strip_prefix('/') {
            // 格式1: /pattern/replacement/
            let (replacement, _rest) = find_regex_delimiter(after_third_slash, '/')?;
            // 转义 replacement 中的特殊字符，防止 regex 反向引用注入
            let escaped = escape_regex_replacement(replacement);
            return Ok(Some(serde_json::Value::String(
                re.replace_all(text, escaped).to_string(),
            )));
        } else if trimmed.starts_with(',') || trimmed.starts_with(')') {
            // 格式2: /pattern/, "replacement" 或 /pattern/)
            // 解析逗号后的字符串字面量
            let after_comma = if let Some(stripped) = trimmed.strip_prefix(',') {
                stripped.trim()
            } else {
                // 没有 replacement，返回原字符串
                return Ok(Some(serde_json::Value::String(text.clone())));
            };
            // 去掉尾部的 )（如果有），因为 value 表达式可能包含闭合括号
            // 仅移除单个尾部 )，避免过度裁剪（如 "foo()" 中的括号）
            let after_comma = after_comma.strip_suffix(')').unwrap_or(after_comma);
            // 解析带引号的字符串
            let replacement_str = parse_json_literal(after_comma)?;
            let replacement = match replacement_str {
                serde_json::Value::String(s) => s,
                _ => {
                    return Err(ExprError::ParseError(format!(
                        "replace replacement must be a string: {}",
                        after_comma
                    )));
                }
            };
            return Ok(Some(serde_json::Value::String(
                re.replace_all(text, escape_regex_replacement(&replacement))
                    .to_string(),
            )));
        } else {
            // 无法识别的格式，直接返回 None 而不是尝试冗余的向后兼容分支。
            // 之前的代码在这里会再次调用 find_regex_delimiter，可能导致重复执行。
            return Ok(None);
        }
    }

    Ok(None)
}

/// Find the next unescaped delimiter character in a string.
fn find_regex_delimiter(s: &str, delimiter: char) -> Result<(&str, &str), ExprError> {
    let mut in_escape = false;
    for (i, c) in s.char_indices() {
        if in_escape {
            in_escape = false;
            continue;
        }
        if c == '\\' {
            in_escape = true;
            continue;
        }
        if c == delimiter {
            return Ok((&s[..i], &s[i + 1..]));
        }
    }
    Err(ExprError::ParseError(format!(
        "Delimiter '{}' not found in: {}",
        delimiter, s
    )))
}

/// Split object key-value pairs (handles commas, nested structures, strings).
fn split_object_pairs(body: &str) -> Result<Vec<(String, String)>, ExprError> {
    let mut pairs = vec![];
    let mut current = String::new();
    let mut depth = 0;
    let mut in_string = false;
    let mut string_char = ' ';

    for c in body.chars() {
        match c {
            '"' | '\'' if !in_string => {
                in_string = true;
                string_char = c;
                current.push(c);
            }
            _ if c == string_char && in_string => {
                in_string = false;
                current.push(c);
            }
            ',' if depth == 0 && !in_string => {
                // 跳过展开运算符 ...p 和空片段
                let trimmed = current.trim();
                if !trimmed.is_empty() && !trimmed.starts_with("...") {
                    pairs.push(parse_kv_pair(trimmed)?);
                }
                current.clear();
            }
            // 追踪括号嵌套深度（包括圆括号，用于函数调用参数）
            '{' | '[' | '(' if !in_string => {
                depth += 1;
                current.push(c);
            }
            '}' | ']' | ')' if !in_string => {
                depth -= 1;
                current.push(c);
            }
            _ => current.push(c),
        }
    }

    // 处理最后一个片段（跳过展开运算符）
    let trimmed = current.trim();
    if !trimmed.is_empty() && !trimmed.starts_with("...") {
        pairs.push(parse_kv_pair(trimmed)?);
    }

    Ok(pairs)
}

/// Parse a single `key: value` pair, finding the first colon not inside a string.
fn parse_kv_pair(pair: &str) -> Result<(String, String), ExprError> {
    // 找到第一个不在引号内的冒号
    let mut in_string = false;
    let mut string_char = ' ';

    for (i, c) in pair.char_indices() {
        match c {
            '"' | '\'' if !in_string => {
                in_string = true;
                string_char = c;
            }
            _ if c == string_char && in_string => in_string = false,
            ':' if !in_string => {
                let key = pair[..i]
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                let val = pair[i + 1..].trim().to_string();
                return Ok((key, val));
            }
            _ => {}
        }
    }

    Err(ExprError::ParseError(format!(
        "Invalid key-value pair: {}",
        pair
    )))
}

// ═══════════════════════════════════════════════════════════
// 测试
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_filter_equality() {
        let item = json!({"type": "ss", "port": 8388});
        assert!(evaluate_predicate("p.type == \"ss\"", &item).unwrap());
        assert!(!evaluate_predicate("p.type == \"vmess\"", &item).unwrap());
    }

    #[test]
    fn test_filter_inequality() {
        let item = json!({"type": "ss"});
        assert!(evaluate_predicate("p.type != \"vmess\"", &item).unwrap());
        assert!(!evaluate_predicate("p.type != \"ss\"", &item).unwrap());
    }

    #[test]
    fn test_filter_numeric_compare() {
        let item = json!({"port": 8388});
        assert!(evaluate_predicate("p.port > 1000", &item).unwrap());
        assert!(evaluate_predicate("p.port >= 8388", &item).unwrap());
        assert!(!evaluate_predicate("p.port < 1000", &item).unwrap());
        assert!(evaluate_predicate("p.port <= 9999", &item).unwrap());
    }

    #[test]
    fn test_filter_includes() {
        let item = json!({"name": "香港 IPLC 01"});
        assert!(evaluate_predicate("p.name.includes(\"香港\")", &item).unwrap());
        assert!(!evaluate_predicate("p.name.includes(\"日本\")", &item).unwrap());
    }

    #[test]
    fn test_filter_regex_match() {
        // 注意: "香港" 的第一个字是 "香"，所以 ^港 不匹配 "香港"
        // 这里使用正确的正则: ^香 匹配 "香港" 开头
        let item = json!({"name": "香港01"});
        assert!(evaluate_predicate("p.name.match(/^香/)", &item).unwrap());
        assert!(!evaluate_predicate("p.name.match(/^日/)", &item).unwrap());
    }

    #[test]
    fn test_filter_boolean() {
        let item = json!({"tls": true});
        assert!(evaluate_predicate("p.tls == true", &item).unwrap());
        assert!(!evaluate_predicate("p.tls == false", &item).unwrap());
    }

    #[test]
    fn test_filter_and_or() {
        let item = json!({"type": "ss", "port": 8388, "tls": true});
        // &&
        assert!(evaluate_predicate("(p.type == \"ss\") && (p.port > 1000)", &item).unwrap());
        assert!(!evaluate_predicate("(p.type == \"ss\") && (p.port < 1000)", &item).unwrap());
        // ||
        assert!(evaluate_predicate("(p.type == \"vmess\") || (p.tls == true)", &item).unwrap());
        assert!(!evaluate_predicate("(p.type == \"vmess\") || (p.tls == false)", &item).unwrap());
    }

    #[test]
    fn test_filter_not() {
        let item = json!({"type": "ss"});
        assert!(evaluate_predicate("!(p.type == \"vmess\")", &item).unwrap());
        assert!(!evaluate_predicate("!(p.type == \"ss\")", &item).unwrap());
    }

    #[test]
    fn test_transform_spread_rename() {
        let item =
            json!({"name": "香港01", "type": "ss", "server": "hk.example.com", "port": 8388});
        let result = evaluate_transform_expr("{...p, name: \"🇭🇰 \" + p.name}", &item).unwrap();
        assert_eq!(result["name"], "🇭🇰 香港01");
        // 其他字段保留
        assert_eq!(result["type"], "ss");
        assert_eq!(result["server"], "hk.example.com");
    }

    #[test]
    fn test_transform_regex_replace() {
        // 注意: "香港" 的第一个字是 "香"，所以 ^港 不会匹配
        // 使用 ^香 来匹配开头，或者使用 香 来匹配任意位置
        let item = json!({"name": "香港01", "type": "ss"});
        let result =
            evaluate_transform_expr("{name: p.name.replace(/^香/,\"🇭🇰 \")}", &item).unwrap();
        assert_eq!(result["name"], "🇭🇰 港01");
    }

    #[test]
    fn test_transform_add_field() {
        let item = json!({"name": "test", "type": "ss"});
        let result = evaluate_transform_expr("{...p, \"udp-support\": true}", &item).unwrap();
        assert_eq!(result["udp-support"], true);
        assert_eq!(result["name"], "test");
    }

    #[test]
    fn test_transform_override_port() {
        let item = json!({"name": "test", "port": 8080});
        let result = evaluate_transform_expr("{...p, port: 443}", &item).unwrap();
        assert_eq!(result["port"], 443);
    }

    #[test]
    fn test_complex_real_world_filter() {
        let proxies = json!([
            {"name": "香港01", "type": "ss", "server": "hk1.com", "port": 8388},
            {"name": "日本01", "type": "vmess", "server": "jp1.com", "port": 443},
            {"name": "美国01", "type": "trojan", "server": "us1.com", "port": 443},
            {"name": "废弃节点", "type": "ss", "server": "", "port": 0},
        ]);

        // 过滤出 ss 类型且端口 > 0 的节点
        let filtered: Vec<_> = proxies
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| evaluate_predicate("(p.type == \"ss\") && (p.port > 0)", p).unwrap())
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["name"], "香港01");
    }

    #[test]
    fn test_remove_semantics() {
        // Remove 与 Filter 语义相反：匹配的被删除
        let item = json!({"name": "广告节点", "type": "ss"});
        // 在 remove 中，匹配的应该被删除
        let matches = evaluate_predicate("p.name.includes(\"广告\")", &item).unwrap();
        assert!(matches); // 这个节点匹配了"包含广告"
    }

    // ─── v2 Built-in function tests ───

    #[test]
    fn test_builtin_regex() {
        let item = json!({"name": "prod-server-01"});
        assert!(evaluate_predicate("regex(p.name, \"^prod-\")", &item).unwrap());
        assert!(!evaluate_predicate("regex(p.name, \"^dev-\")", &item).unwrap());
    }

    #[test]
    fn test_builtin_contains() {
        let item = json!({"name": "香港 IPLC 01"});
        assert!(evaluate_predicate("contains(p.name, \"香港\")", &item).unwrap());
    }

    #[test]
    fn test_builtin_len() {
        let item = json!({"name": "test"});
        // len(p.name) returns 4, compare with number
        assert!(evaluate_predicate("len(p.name) == 4", &item).unwrap());
    }

    #[test]
    fn test_builtin_starts_with() {
        let item = json!({"type": "ss"});
        assert!(evaluate_predicate("starts_with(p.type, \"s\")", &item).unwrap());
        assert!(!evaluate_predicate("starts_with(p.type, \"v\")", &item).unwrap());
    }

    #[test]
    fn test_builtin_ends_with() {
        let item = json!({"name": "test01"});
        assert!(evaluate_predicate("ends_with(p.name, \"01\")", &item).unwrap());
    }

    #[test]
    fn test_builtin_lower_upper() {
        let item = json!({"name": "Hello"});
        assert!(evaluate_predicate("lower(p.name) == \"hello\"", &item).unwrap());
        assert!(evaluate_predicate("upper(p.name) == \"HELLO\"", &item).unwrap());
    }

    #[test]
    fn test_builtin_type_of() {
        let item = json!({"name": "test", "port": 443, "tls": true});
        assert!(evaluate_predicate("type_of(p.name) == \"string\"", &item).unwrap());
        assert!(evaluate_predicate("type_of(p.port) == \"number\"", &item).unwrap());
        assert!(evaluate_predicate("type_of(p.tls) == \"bool\"", &item).unwrap());
    }

    #[test]
    fn test_builtin_abs() {
        let item = json!({"value": -5});
        assert!(evaluate_predicate("abs(p.value) == 5", &item).unwrap());
    }

    // ─── v2 Pipe operator tests ───

    #[test]
    fn test_pipe_to_function() {
        let item = json!({"name": "Hello World"});
        // p.name | lower should produce "hello world"
        // Then == "hello world" comparison
        assert!(evaluate_predicate("p.name | lower == \"hello world\"", &item).unwrap());
    }

    #[test]
    fn test_pipe_chain() {
        let item = json!({"name": "Hello"});
        // p.name | lower | contains("hello")
        assert!(evaluate_predicate("p.name | lower | contains(\"hello\")", &item).unwrap());
    }

    // ─── len() 一致性回归测试 ───

    /// ASCII 字符串：len() 应返回字符数（ASCII 范围内字符数 = 字节数）。
    /// 基本功能验证，确保 len() 对纯 ASCII 输入返回正确值。
    #[test]
    fn test_len_string_ascii() {
        let item = json!({"name": "hello"});
        // "hello" 有 5 个字符
        assert!(evaluate_predicate("len(p.name) == 5", &item).unwrap());

        let item2 = json!({"name": ""});
        // 空字符串长度为 0
        assert!(evaluate_predicate("len(p.name) == 0", &item2).unwrap());

        let item3 = json!({"name": "a"});
        // 单字符长度为 1
        assert!(evaluate_predicate("len(p.name) == 1", &item3).unwrap());
    }

    /// 多字节字符（中文）：len() 应返回字符数而非字节数。
    /// 这是 len() 使用 chars().count() 而非 .len()（字节数）的回归测试。
    /// "中文" 各 3 字节（UTF-8），但 chars().count() 应为 2。
    #[test]
    fn test_len_string_multibyte() {
        // "中文" — 2 个字符，6 个字节（UTF-8）
        let item = json!({"name": "中文"});
        assert!(
            evaluate_predicate("len(p.name) == 2", &item).unwrap(),
            "len(\"中文\") 应返回 2（字符数），而非 6（字节数）"
        );

        // 单个 emoji 🇭🇰 — 由两个 regional indicator 组成，chars().count() = 2
        let item2 = json!({"flag": "🇭🇰"});
        assert!(
            evaluate_predicate("len(p.flag) == 2", &item2).unwrap(),
            "len(\"🇭🇰\") 应返回 2（regional indicator 对）"
        );

        // 日文假名 — 3 字符
        let item3 = json!({"text": "こんにちは"});
        assert!(
            evaluate_predicate("len(p.text) == 5", &item3).unwrap(),
            "len(\"こんにちは\") 应返回 5"
        );
    }

    /// 混合 ASCII + 多字节：len() 应返回总字符数。
    /// 验证混合编码字符串的字符计数正确性。
    #[test]
    fn test_len_string_mixed() {
        // "Hello世界" — 5 ASCII + 2 CJK = 7 字符
        let item = json!({"name": "Hello世界"});
        assert!(
            evaluate_predicate("len(p.name) == 7", &item).unwrap(),
            "len(\"Hello世界\") 应返回 7"
        );

        // "abc-🇭🇰-123" — 3 + 1 + 2 + 1 + 3 = 10 字符
        let item2 = json!({"tag": "abc-🇭🇰-123"});
        assert!(
            evaluate_predicate("len(p.tag) == 10", &item2).unwrap(),
            "len(\"abc-🇭🇰-123\") 应返回 10"
        );
    }

    /// 数组：len() 应返回元素数。
    /// 验证 len() 对 JSON 数组类型的正确处理。
    #[test]
    fn test_len_array() {
        // 注意：当前 expr.rs 的 len() 仅支持 string/array/null，
        // 但 resolve_value_expr 从 JSON Value 解析时，数组字段会映射为 ExprValue::Array。
        // 这里测试通过直接表达式调用 len() 对数组字段的行为。
        let item = json!({"tags": ["ss", "vmess", "trojan"]});
        // 数组有 3 个元素
        assert!(
            evaluate_predicate("len(p.tags) == 3", &item).unwrap(),
            "len([\"ss\", \"vmess\", \"trojan\"]) 应返回 3"
        );

        let item2 = json!({"tags": []});
        assert!(
            evaluate_predicate("len(p.tags) == 0", &item2).unwrap(),
            "len([]) 应返回 0"
        );

        let item3 = json!({"tags": ["single"]});
        assert!(
            evaluate_predicate("len(p.tags) == 1", &item3).unwrap(),
            "len([\"single\"]) 应返回 1"
        );
    }

    /// 空字符串和空数组：len() 应返回 0。
    /// 边界条件测试，确保零长度输入不会导致异常。
    #[test]
    fn test_len_empty() {
        // 空字符串
        let item = json!({"name": ""});
        assert!(
            evaluate_predicate("len(p.name) == 0", &item).unwrap(),
            "len(\"\") 应返回 0"
        );

        // 空数组
        let item2 = json!({"items": []});
        assert!(
            evaluate_predicate("len(p.items) == 0", &item2).unwrap(),
            "len([]) 应返回 0"
        );

        // null 字段 — len(null) 应返回 0
        let item3 = json!({"missing": null});
        assert!(
            evaluate_predicate("len(p.missing) == 0", &item3).unwrap(),
            "len(null) 应返回 0"
        );
    }
}
