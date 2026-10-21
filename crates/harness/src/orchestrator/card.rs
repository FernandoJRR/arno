//! Decision card rows from a backend's preview of the frozen payload
//! (SPEC §11 #27). Generic over any JSON result (AGENTS.md #1); display
//! only, never dispatched (AGENTS.md #3).

use super::args::is_uuid;
use contract::DecisionDetail;
use serde_json::Value;

const MAX_ROWS: usize = 12;
const MAX_JOINED: usize = 5;
const DEFAULT_MARK: &str = " (default)";

/// One row per shown leaf of `preview`. A wrapper (the only object, holding
/// more fields than the rest) is unwrapped and shown first; ids and empty
/// values are dropped. A leaf named like a tool parameter that `args` does
/// not carry is marked `(default)`: the backend filled it in. Keys in `hide`
/// (the manifest's `card_hide`) are left out unless they are parameters.
/// Empty when nothing is worth showing.
pub fn preview_details(
    preview: &Value,
    args: &Value,
    params: &Value,
    hide: &[String],
) -> Vec<DecisionDetail> {
    let Some(top) = preview.as_object() else {
        return Vec::new();
    };
    let is_param = |key: &str| !params["properties"][key].is_null();
    let mark = |key: &str| is_param(key) && args.get(key).is_none_or(Value::is_null);
    let hidden = |key: &str| !is_param(key) && hide.iter().any(|h| h == key);
    let view = View {
        mark: &mark,
        hidden: &hidden,
    };
    let mut rows = Vec::new();
    let mut wrappers = top.iter().filter(|(_, v)| v.is_object());
    match (wrappers.next(), wrappers.next()) {
        (Some((key, inner)), None)
            if inner.as_object().map_or(0, serde_json::Map::len) > top.len() - 1 =>
        {
            walk(inner, &[], &view, &mut rows);
            let rest: serde_json::Map<String, Value> = top
                .iter()
                .filter(|(k, _)| *k != key)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            walk(&Value::Object(rest), &[], &view, &mut rows);
        }
        _ => walk(preview, &[], &view, &mut rows),
    }
    rows.truncate(MAX_ROWS);
    rows
}

/// Per-key rules for one card.
struct View<'a> {
    /// Filled in by the backend: shown with `(default)`.
    mark: &'a dyn Fn(&str) -> bool,
    hidden: &'a dyn Fn(&str) -> bool,
}

fn walk(v: &Value, path: &[&str], view: &View, rows: &mut Vec<DecisionDetail>) {
    let Some(obj) = v.as_object() else { return };
    for (k, child) in obj {
        if (view.hidden)(k) {
            continue;
        }
        let mut here = path.to_vec();
        here.push(k);
        if child.is_object() {
            walk(child, &here, view, rows);
        } else if let Some(mut value) = render(child) {
            if (view.mark)(k) {
                value.push_str(DEFAULT_MARK);
            }
            rows.push(DecisionDetail {
                label: label(&here),
                value,
            });
        }
    }
}

fn render(v: &Value) -> Option<String> {
    match v {
        Value::Null | Value::Object(_) => None,
        Value::String(s) if s.trim().is_empty() || is_uuid(s) => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(_) | Value::Number(_) => Some(v.to_string()),
        Value::Array(items) => {
            let shown: Option<Vec<String>> = items
                .iter()
                .map(|i| match i {
                    Value::String(s) if !is_uuid(s) => Some(s.clone()),
                    Value::Number(_) | Value::Bool(_) => Some(i.to_string()),
                    _ => None,
                })
                .collect();
            match shown {
                Some(list) if !list.is_empty() && list.len() <= MAX_JOINED => Some(list.join(", ")),
                _ => Some(items.len().to_string()),
            }
        }
    }
}

/// `["target_category", "name"]` → "Target category name".
fn label(path: &[&str]) -> String {
    let text = path.join(" ").replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "d65cf7f5-6fcc-4ba1-a6e6-69fa850c4444";

    fn rows(preview: &Value, args: &Value, params: &Value) -> Vec<(String, String)> {
        preview_details(preview, args, params, &[])
            .into_iter()
            .map(|d| (d.label, d.value))
            .collect()
    }

    fn create_params() -> Value {
        json!({"properties": {"description": {}, "amount": {}, "account_id": {},
                              "category_id": {}, "date": {}, "apply": {}}})
    }

    fn create_preview() -> Value {
        json!({"kind": "create_transaction", "proposed": {
            "account_id": ID, "account_name": "Banco Industrial", "amount": 250.0,
            "category_name": "Food & Dining", "date": "2026-10-19",
            "description": "Tacos", "notes": null}})
    }

    #[test]
    fn a_wrapped_preview_shows_names_and_marks_backend_defaults() {
        let args = json!({"description": "Tacos", "amount": 250, "account_id": ID});
        assert_eq!(
            rows(&create_preview(), &args, &create_params()),
            [
                ("Account name", "Banco Industrial"),
                ("Amount", "250.0"),
                ("Category name", "Food & Dining"),
                ("Date", "2026-10-19 (default)"),
                ("Description", "Tacos"),
                ("Kind", "create_transaction"),
            ]
            .map(|(l, v)| (l.to_owned(), v.to_owned()))
        );
    }

    #[test]
    fn a_given_argument_is_not_marked() {
        let args = json!({"description": "Tacos", "date": "2026-10-19"});
        let shown = rows(&create_preview(), &args, &create_params());
        assert!(shown.contains(&("Date".into(), "2026-10-19".into())));
    }

    #[test]
    fn nested_paths_become_labels_and_id_lists_are_counted() {
        let preview = json!({"affected": [], "affected_count": 0, "missing_ids": [ID],
                             "target_category": {"id": ID, "name": "Food & Dining"},
                             "tags": ["a", "b"]});
        let shown = rows(&preview, &json!({}), &json!({}));
        assert!(shown.contains(&("Target category name".into(), "Food & Dining".into())));
        assert!(shown.contains(&("Affected count".into(), "0".into())));
        assert!(shown.contains(&("Affected".into(), "0".into())));
        assert!(shown.contains(&("Missing ids".into(), "1".into())));
        assert!(shown.contains(&("Tags".into(), "a, b".into())));
    }

    #[test]
    fn hidden_keys_are_dropped_unless_they_are_parameters() {
        let hide = ["kind".to_owned(), "date".to_owned()];
        let shown: Vec<String> =
            preview_details(&create_preview(), &json!({}), &create_params(), &hide)
                .into_iter()
                .map(|d| d.label)
                .collect();
        assert!(!shown.contains(&"Kind".to_owned()), "{shown:?}");
        assert!(
            shown.contains(&"Date".to_owned()),
            "a parameter stays: {shown:?}"
        );
    }

    #[test]
    fn non_objects_show_nothing_and_rows_are_capped() {
        assert!(rows(&json!("ok"), &json!({}), &json!({})).is_empty());
        let many: serde_json::Map<String, Value> =
            (0..20).map(|i| (format!("k{i:02}"), json!(i))).collect();
        assert_eq!(
            rows(&Value::Object(many), &json!({}), &json!({})).len(),
            MAX_ROWS
        );
    }
}
