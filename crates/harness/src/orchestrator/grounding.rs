//! Output-side number hygiene for final answers (SPEC §11 #20), all
//! deterministic and tool-agnostic (AGENTS.md #1):
//! - `checks` a backend put in a tool result are relayed verbatim (§4.2);
//! - numbers the answer states that no tool result or user message contains
//!   (and that aren't a sum or difference of two such numbers) are named.

use serde_json::Value;
use std::collections::HashSet;

/// Units of 1e-5, matching the precision tool results report.
const SCALE: f64 = 100_000.0;
/// Integers below this are list numbering, counts, "1 of 2" — never flagged.
const MIN_FLAGGED: i64 = 10 * SCALE as i64;
const MAX_CHECKS: usize = 5;
const MAX_CHECK_CHARS: usize = 300;
const MAX_UNGROUNDED: usize = 5;

/// Top-level `checks: [string]` of one tool result, which arrives either as
/// a JSON object or as a string holding one (MCP text content).
pub fn tool_checks(result: &Value) -> Vec<String> {
    let parsed;
    let obj = match result {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => {
                parsed = v;
                &parsed
            }
            Err(_) => return Vec::new(),
        },
        v => v,
    };
    obj.get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|c| c.chars().take(MAX_CHECK_CHARS).collect())
        .collect()
}

/// The result as the model sees it: without `checks` (SPEC §4.2 — they are
/// relayed to the user, and the model would only repeat them). Same shape
/// back: an object stays an object, a JSON string stays a string.
pub fn strip_checks(result: &Value) -> Value {
    match result {
        Value::Object(map) if map.contains_key("checks") => {
            let mut map = map.clone();
            map.remove("checks");
            Value::Object(map)
        }
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(mut map)) if map.contains_key("checks") => {
                map.remove("checks");
                Value::String(Value::Object(map).to_string())
            }
            _ => result.clone(),
        },
        _ => result.clone(),
    }
}

/// Adds `checks` to `acc`, deduplicated and capped per order.
pub fn collect_checks(acc: &mut Vec<String>, result: &Value) {
    for c in tool_checks(result) {
        if acc.len() < MAX_CHECKS && !acc.contains(&c) {
            acc.push(c);
        }
    }
}

/// Numbers in `answer` that no source mentions, in order of appearance.
pub fn ungrounded(answer: &str, sources: &[&str]) -> Vec<String> {
    let known: HashSet<i64> = sources
        .iter()
        .flat_map(|s| numbers(s))
        .flat_map(|(_, vs)| vs)
        .collect();
    let mut out: Vec<String> = Vec::new();
    for (raw, values) in numbers(answer) {
        let grounded = values.iter().any(|&x| {
            x.abs() < MIN_FLAGGED
                || known.contains(&x)
                || known
                    .iter()
                    .any(|&a| known.contains(&(x - a)) || known.contains(&(a - x)))
        });
        if !grounded && !out.contains(&raw) && out.len() < MAX_UNGROUNDED {
            out.push(raw);
        }
    }
    out
}

/// The answer as delivered: model text, then relayed checks, then the
/// not-found note.
pub fn decorate(text: &str, checks: &[String], ungrounded: &[String]) -> String {
    let mut out = text.to_owned();
    if !checks.is_empty() {
        out.push_str("\n\n⚠ Checks:");
        for c in checks {
            out.push_str("\n- ");
            out.push_str(c);
        }
    }
    if !ungrounded.is_empty() {
        out.push_str("\n\n⚠ Not found in the source data: ");
        out.push_str(&ungrounded.join(", "));
    }
    out
}

/// Numeric tokens with every plausible reading (a lone comma may be a
/// decimal or a thousands separator), scaled by `SCALE`.
fn numbers(text: &str) -> Vec<(String, Vec<i64>)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == ',') {
            i += 1;
        }
        let mut end = i;
        while end > start && !chars[end - 1].is_ascii_digit() {
            end -= 1;
        }
        let raw: String = chars[start..end].iter().collect();
        let values = readings(&raw);
        if !values.is_empty() {
            out.push((raw, values));
        }
    }
    out
}

fn readings(raw: &str) -> Vec<i64> {
    let dots = raw.matches('.').count();
    let commas = raw.matches(',').count();
    let candidates: Vec<String> = match (dots, commas) {
        (0, 0) | (1, 0) => vec![raw.to_owned()],
        (_, 0) => vec![raw.replace('.', "")],
        (0, 1) => vec![raw.replace(',', "."), raw.replace(',', "")],
        (0, _) => vec![raw.replace(',', "")],
        _ => {
            let last = raw.rfind(['.', ',']).unwrap_or(0);
            let (int, frac) = raw.split_at(last);
            vec![format!("{}.{}", int.replace(['.', ','], ""), &frac[1..])]
        }
    };
    candidates
        .into_iter()
        .filter_map(|c| c.parse::<f64>().ok())
        .filter(|v| v.abs() < 1e12)
        .map(|v| (v * SCALE).round() as i64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OCR: &str = r#"{"text":"Item Q 70.46\nFee Q10\nTotal a Pagar Q 30.46\nLectura 5114\nFecha 26/08/2026\nTarifa 1,234.50"}"#;

    #[test]
    fn invented_numbers_are_named() {
        let answer = "Total: Q30.46, or Q38.01 with tax.";
        assert_eq!(ungrounded(answer, &[OCR]), vec!["38.01"]);
    }

    #[test]
    fn copied_reformatted_and_derived_numbers_are_grounded() {
        let answer = "Item Q70.46 plus fee Q10 = Q80.46 (shown as 30.46). Reading 5,114 \
                      on August 26, 2026; tariff 1234.5; difference 40.";
        assert!(
            ungrounded(answer, &[OCR]).is_empty(),
            "{:?}",
            ungrounded(answer, &[OCR])
        );
    }

    #[test]
    fn small_integers_are_never_flagged() {
        assert!(ungrounded("1. first\n2. second, 3 items", &["nothing"]).is_empty());
    }

    #[test]
    fn checks_are_read_from_objects_and_json_strings() {
        let obj = json!({"text": "x", "checks": ["a", 5, "b"]});
        assert_eq!(tool_checks(&obj), vec!["a", "b"]);
        let s = Value::String(r#"{"checks":["c"]}"#.into());
        assert_eq!(tool_checks(&s), vec!["c"]);
        assert!(tool_checks(&Value::String("plain text".into())).is_empty());
        assert!(tool_checks(&json!({"rows": []})).is_empty());

        let mut acc = Vec::new();
        for _ in 0..3 {
            collect_checks(&mut acc, &obj);
        }
        assert_eq!(acc, vec!["a", "b"], "deduplicated across tool calls");
    }

    #[test]
    fn strip_checks_keeps_shape_and_everything_else() {
        let obj = json!({"text": "x", "checks": ["a"]});
        assert_eq!(strip_checks(&obj), json!({"text": "x"}));
        let s = Value::String(r#"{"text":"x","checks":["a"]}"#.into());
        let stripped = strip_checks(&s);
        let back: Value = serde_json::from_str(stripped.as_str().unwrap()).unwrap();
        assert_eq!(back, json!({"text": "x"}));
        let plain = Value::String("not json".into());
        assert_eq!(strip_checks(&plain), plain);
        let other = json!({"rows": [1]});
        assert_eq!(strip_checks(&other), other);
    }

    #[test]
    fn decorate_appends_checks_then_note() {
        let out = decorate(
            "Total Q30.46.",
            &["\"30.46\" may be 80.46".into()],
            &["38.01".into()],
        );
        assert_eq!(
            out,
            "Total Q30.46.\n\n⚠ Checks:\n- \"30.46\" may be 80.46\n\n⚠ Not found in the source data: 38.01"
        );
        assert_eq!(decorate("ok", &[], &[]), "ok");
    }
}
