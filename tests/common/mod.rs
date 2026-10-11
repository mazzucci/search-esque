//! Shared by the integration tests: a comparison of two JSON values that treats numbers as
//! numbers (5 equals 5.0) and allows a relative difference of 1e-9 on floats, since the two
//! engines sum floating-point values in different orders. Everything else is exact.

use serde_json::Value;

pub fn close(a: &Value, b: &Value) -> bool {
    diffs(a, b, "", &mut Vec::new())
}

/// Like `close`, but says where the two differ.
pub fn explain(a: &Value, b: &Value) -> Vec<String> {
    let mut out = Vec::new();
    diffs(a, b, "", &mut out);
    out
}

fn diffs(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (
                x.as_f64().unwrap_or(f64::NAN),
                y.as_f64().unwrap_or(f64::NAN),
            );
            let ok = (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0);
            if !ok {
                out.push(format!("{}: {} != {}", path, x, y));
            }
            ok
        }
        (Value::Object(x), Value::Object(y)) => {
            let mut ok = true;
            for k in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
                let p = format!("{}.{}", path, k);
                match (x.get(k), y.get(k)) {
                    (Some(xv), Some(yv)) => ok &= diffs(xv, yv, &p, out),
                    (xv, _) => {
                        out.push(format!(
                            "{}: only on the {} side",
                            p,
                            if xv.is_some() { "left" } else { "right" }
                        ));
                        ok = false;
                    }
                }
            }
            ok
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!("{}: {} items vs {}", path, x.len(), y.len()));
                return false;
            }
            let mut ok = true;
            for (i, (xv, yv)) in x.iter().zip(y).enumerate() {
                ok &= diffs(xv, yv, &format!("{}[{}]", path, i), out);
            }
            ok
        }
        _ => {
            if a != b {
                out.push(format!("{}: {} != {}", path, a, b));
            }
            a == b
        }
    }
}
