//! Minimal CSV serializer for extracted item arrays. Mirrors the extension's
//! `toCsv` (popup.js): header = union of item keys in first-seen order; values
//! quoted only when they contain a comma, quote, or newline.

use serde_json::Value;

pub fn to_csv(items: &[Value]) -> String {
    let mut keys: Vec<String> = Vec::new();
    for it in items {
        if let Some(obj) = it.as_object() {
            for k in obj.keys() {
                if !keys.iter().any(|x| x == k) {
                    keys.push(k.clone());
                }
            }
        }
    }

    let mut out = String::new();
    out.push_str(&keys.iter().map(|k| esc(k)).collect::<Vec<_>>().join(","));
    out.push('\n');

    for it in items {
        let row: Vec<String> = keys
            .iter()
            .map(|k| {
                let cell = match it.get(k) {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                };
                esc(&cell)
            })
            .collect();
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

fn esc(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
