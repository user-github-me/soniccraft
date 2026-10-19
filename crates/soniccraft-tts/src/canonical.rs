//! Canonical JSON for content hashing (DESIGN.md §5.1).
//!
//! The workspace enables `serde_json/preserve_order`, so [`serde_json::Value`] objects keep insertion order and
//! `serde_json::to_string` is *not* canonical. [`canonical_json`] writes:
//!
//! - object keys sorted recursively (by UTF-8 bytes, which equals code point order);
//! - no insignificant whitespace;
//! - numbers normalized: integers as integers; floats rounded to 6 decimals, `-0.0` as `0`, integral floats
//!   as integers (`1.0` and `1` hash the same); non-finite numbers rejected;
//! - strings escaped exactly as `serde_json` escapes them.

use std::fmt::Write as _;

use serde_json::{Number, Value};

/// A value that has no canonical JSON form.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CanonicalJsonError {
    /// NaN or ±infinity (only reachable through `arbitrary_precision` numbers).
    #[error("non-finite number `{0}` cannot be hashed")]
    NonFinite(String),
}

/// Integral floats with a magnitude below this are written as integers (2^53: exactly representable).
const MAX_EXACT_INT: f64 = 9_007_199_254_740_992.0;

/// Canonical JSON text of `value` (see the module docs).
pub fn canonical_json(value: &Value) -> Result<String, CanonicalJsonError> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

/// Round a float to 6 decimals and map `-0.0` to `0.0`. Rejects NaN and infinities.
pub fn normalize_f64(x: f64) -> Result<f64, CanonicalJsonError> {
    if !x.is_finite() {
        return Err(CanonicalJsonError::NonFinite(x.to_string()));
    }
    // Beyond 1e15 every f64 is already integral at micro precision; scaling would only lose bits.
    let rounded = if x.abs() < 1e15 {
        (x * 1e6).round() / 1e6
    } else {
        x
    };
    Ok(if rounded == 0.0 { 0.0 } else { rounded })
}

fn write_value(out: &mut String, value: &Value) -> Result<(), CanonicalJsonError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n)?,
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (i, (k, v)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, v)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_number(out: &mut String, n: &Number) -> Result<(), CanonicalJsonError> {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else {
        let f = n
            .as_f64()
            .ok_or_else(|| CanonicalJsonError::NonFinite(n.to_string()))?;
        let f = normalize_f64(f)?;
        if f.fract() == 0.0 && f.abs() < MAX_EXACT_INT {
            let _ = write!(out, "{}", f as i64);
        } else {
            // `Display` for f64 is the shortest round-tripping decimal and never uses exponent notation.
            let _ = write!(out, "{f}");
        }
    }
    Ok(())
}

fn write_string(out: &mut String, s: &str) {
    // Serializing a &str cannot fail; the fallback keeps this total without unwrap.
    match serde_json::to_string(s) {
        Ok(quoted) => out.push_str(&quoted),
        Err(_) => {
            out.push('"');
            out.push_str(&s.escape_default().to_string());
            out.push('"');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_recursively_and_strips_whitespace() {
        let v = json!({"b": 1, "a": {"z": [3, {"y": 1, "x": 2}], "m": null}, "é": true, "A": "q\"uote"});
        assert_eq!(
            canonical_json(&v).unwrap(),
            r#"{"A":"q\"uote","a":{"m":null,"z":[3,{"x":2,"y":1}]},"b":1,"é":true}"#
        );
    }

    #[test]
    fn insertion_order_does_not_matter() {
        let mut m1 = serde_json::Map::new();
        m1.insert("stability".into(), json!(0.5));
        m1.insert("speed".into(), json!(1.1));
        let mut m2 = serde_json::Map::new();
        m2.insert("speed".into(), json!(1.1));
        m2.insert("stability".into(), json!(0.5));
        // preserve_order is on: the maps differ in iteration order...
        assert_ne!(
            serde_json::to_string(&m1).unwrap(),
            serde_json::to_string(&m2).unwrap()
        );
        // ...but not canonically.
        assert_eq!(
            canonical_json(&Value::Object(m1)).unwrap(),
            canonical_json(&Value::Object(m2)).unwrap()
        );
    }

    #[test]
    fn normalizes_numbers() {
        assert_eq!(
            canonical_json(&json!([1.0, 1, -0.0, 0.1234567, 2.5e-7, 0.1, 1e300])).unwrap(),
            {
                let big = format!("{}", 1e300f64);
                format!("[1,1,0,0.123457,0,0.1,{big}]")
            }
        );
        assert_eq!(canonical_json(&json!(-3)).unwrap(), "-3");
        assert_eq!(
            canonical_json(&json!(u64::MAX)).unwrap(),
            u64::MAX.to_string()
        );
        assert_eq!(normalize_f64(-0.0).unwrap().to_bits(), 0.0f64.to_bits());
        assert!(normalize_f64(f64::NAN).is_err());
        assert!(normalize_f64(f64::INFINITY).is_err());
    }
}
