//! Schema-driven argument hygiene before dispatch (SPEC §11 #26): small
//! models fill every field they see, often with placeholders the backend
//! rejects. Generic over any JSON-schema tool (AGENTS.md #1).

use serde_json::{Map, Value};

pub struct Cleaned {
    pub args: Value,
    /// Required parameters that are absent or empty, then any parameter
    /// whose value breaks its declared `format`.
    pub missing: Vec<String>,
}

/// Drops empty optional arguments; reports empty required ones and values
/// that break a declared `uuid`/`date` format (`"default"` for an id).
/// `params` is the tool's `parameters` schema.
pub fn clean(args: &Value, params: &Value) -> Cleaned {
    let Some(obj) = args.as_object() else {
        return Cleaned {
            args: args.clone(),
            missing: Vec::new(),
        };
    };
    let required = required(params);
    let mut kept = Map::new();
    let mut invalid = Vec::new();
    for (k, v) in obj {
        let schema = &params["properties"][k];
        if is_empty(v, schema) {
            continue;
        }
        if breaks_format(v, schema) {
            invalid.push(k.clone());
        } else {
            kept.insert(k.clone(), v.clone());
        }
    }
    let mut missing: Vec<String> = required
        .into_iter()
        .filter(|r| !kept.contains_key(r))
        .collect();
    for k in invalid {
        if !missing.contains(&k) {
            missing.push(k);
        }
    }
    Cleaned {
        args: Value::Object(kept),
        missing,
    }
}

/// Placeholder-shaped: null, `""`, `{}`, an array below `minItems` (at
/// least 1), or an object with an empty required property. `false` and `0`
/// are real values.
fn is_empty(v: &Value, schema: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => (a.len() as u64) < schema["minItems"].as_u64().unwrap_or(1).max(1),
        Value::Object(o) => {
            o.is_empty()
                || required(schema).iter().any(|r| {
                    o.get(r)
                        .is_none_or(|child| is_empty(child, &schema["properties"][r]))
                })
        }
        Value::Bool(_) | Value::Number(_) => false,
    }
}

fn breaks_format(v: &Value, schema: &Value) -> bool {
    let Some(s) = v.as_str() else { return false };
    match schema["format"].as_str() {
        Some("uuid") => !is_uuid(s),
        Some("date") => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_err(),
        _ => false,
    }
}

pub fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

fn required(schema: &Value) -> Vec<String> {
    schema["required"]
        .as_array()
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Tools among `candidates` whose (unnamespaced) name mentions the
/// parameter's stem: `account_id` → `…list_accounts`.
pub fn lookup_hint<'a>(param: &str, candidates: impl Iterator<Item = &'a str>) -> Vec<String> {
    let stem = param
        .strip_suffix("_ids")
        .or_else(|| param.strip_suffix("_id"))
        .unwrap_or(param);
    let stem = stem.strip_suffix(['y', 's']).unwrap_or(stem);
    if stem.len() < 3 {
        return Vec::new();
    }
    candidates
        .filter(|name| name.rsplit('.').next().is_some_and(|n| n.contains(stem)))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params() -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {"type": "string"},
                "amount": {"type": "number"},
                "account_id": {"type": "string"},
                "group_id": {"type": "string"},
                "notes": {"type": "string"},
                "apply": {"type": "boolean"},
                "splits": {
                    "type": "object",
                    "properties": {
                        "share_type": {"type": "string"},
                        "members": {"type": "array", "minItems": 1}
                    },
                    "required": ["share_type", "members"]
                }
            },
            "required": ["description", "amount", "account_id"]
        })
    }

    #[test]
    fn drops_empty_optionals_and_keeps_real_values() {
        let c = clean(
            &json!({"description": "Tacos", "amount": 0, "account_id": "a1",
                    "group_id": "", "notes": null, "apply": false,
                    "splits": {"share_type": "equal", "members": []}}),
            &params(),
        );
        assert_eq!(
            c.args,
            json!({"description": "Tacos", "amount": 0, "account_id": "a1", "apply": false})
        );
        assert!(c.missing.is_empty());
    }

    #[test]
    fn keeps_a_complete_nested_object() {
        let splits = json!({"share_type": "equal", "members": [{"member_id": "m"}]});
        let c = clean(
            &json!({"description": "x", "amount": 1, "account_id": "a", "splits": splits}),
            &params(),
        );
        assert_eq!(c.args["splits"], splits);
    }

    #[test]
    fn reports_empty_or_absent_required_arguments() {
        let c = clean(&json!({"description": "x", "account_id": " "}), &params());
        assert_eq!(
            c.missing,
            vec!["amount".to_owned(), "account_id".to_owned()]
        );
        assert!(c.args.get("account_id").is_none());
    }

    #[test]
    fn values_breaking_a_declared_format_are_reported() {
        let params = json!({"type": "object", "properties": {
            "account_id": {"type": "string", "format": "uuid"},
            "category_id": {"type": "string", "format": "uuid"},
            "date": {"type": "string", "format": "date"}},
            "required": ["account_id"]});
        let c = clean(
            &json!({"account_id": "default", "category_id": "food", "date": "yesterday"}),
            &params,
        );
        assert_eq!(c.missing, vec!["account_id", "category_id", "date"]);
        let ok = clean(
            &json!({"account_id": "d65cf7f5-6fcc-4ba1-a6e6-69fa850c4444", "date": "2026-10-18"}),
            &params,
        );
        assert!(ok.missing.is_empty());
        assert_eq!(ok.args["date"], "2026-10-18");
    }

    #[test]
    fn schemaless_arguments_lose_only_placeholders() {
        let c = clean(&json!({"a": "", "b": "x", "c": {}}), &json!({}));
        assert_eq!(c.args, json!({"b": "x"}));
    }

    #[test]
    fn lookup_hint_matches_the_parameter_stem() {
        let tools = [
            "fin.list_accounts",
            "fin.list_categories",
            "fin.list_transactions",
        ];
        assert_eq!(
            lookup_hint("account_id", tools.iter().copied()),
            vec!["fin.list_accounts"]
        );
        assert_eq!(
            lookup_hint("category_id", tools.iter().copied()),
            vec!["fin.list_categories"]
        );
        assert_eq!(
            lookup_hint("transaction_ids", tools.iter().copied()),
            vec!["fin.list_transactions"]
        );
        assert!(lookup_hint("id", tools.iter().copied()).is_empty());
    }
}
