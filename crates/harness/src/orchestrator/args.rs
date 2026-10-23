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

/// Copies into `args` each argument of `prev` (an earlier call that missed
/// a required argument) that `args` leaves absent or empty (SPEC §11 #26).
/// Returns the keys filled.
pub fn carry_over(prev: &Value, args: &mut Value, params: &Value) -> Vec<String> {
    let (Some(prev), Some(args)) = (prev.as_object(), args.as_object_mut()) else {
        return Vec::new();
    };
    let mut filled = Vec::new();
    for (k, v) in prev {
        if args
            .get(k)
            .is_none_or(|cur| is_empty(cur, &params["properties"][k]))
        {
            args.insert(k.clone(), v.clone());
            filled.push(k.clone());
        }
    }
    filled
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

/// The one date parameter of a tool, if it has exactly one: `format: date`,
/// or a string named `date` / `*_date` (SPEC §11 #30).
pub fn date_param(params: &Value) -> Option<String> {
    let props = params["properties"].as_object()?;
    let mut dates = props.iter().filter(|(k, schema)| {
        schema["format"] == "date"
            || (schema["type"] == "string" && (*k == "date" || k.ends_with("_date")))
    });
    match (dates.next(), dates.next()) {
        (Some((k, _)), None) => Some(k.clone()),
        _ => None,
    }
}

/// The one distinct date written in `texts` (SPEC §11 #30): ISO
/// `YYYY-MM-DD`, or `D/M/YYYY` (also `-` or `.`) when the day and month
/// cannot be swapped. None when there is no date or more than one.
pub fn attachment_date(texts: &[&str]) -> Option<chrono::NaiveDate> {
    let mut found: Vec<chrono::NaiveDate> = Vec::new();
    for text in texts {
        let tokens = text.split(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '/' | '.')));
        for token in tokens {
            if let Some(d) = parse_date(token.trim_matches(|c| matches!(c, '-' | '/' | '.')))
                && !found.contains(&d)
            {
                found.push(d);
            }
        }
    }
    match found.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

fn parse_date(token: &str) -> Option<chrono::NaiveDate> {
    let sep = token.chars().find(|c| !c.is_ascii_digit())?;
    let parts: Vec<&str> = token.split(sep).collect();
    let [a, b, c] = parts.as_slice() else {
        return None;
    };
    if ![a, b, c]
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|ch| ch.is_ascii_digit()))
    {
        return None;
    }
    let num = |p: &str| p.parse::<u32>().ok();
    if a.len() == 4 && sep == '-' {
        return chrono::NaiveDate::from_ymd_opt(a.parse().ok()?, num(b)?, num(c)?);
    }
    if c.len() != 4 || a.len() > 2 || b.len() > 2 {
        return None;
    }
    let year = c.parse().ok()?;
    let day_first = chrono::NaiveDate::from_ymd_opt(year, num(b)?, num(a)?);
    let month_first = chrono::NaiveDate::from_ymd_opt(year, num(a)?, num(b)?);
    match (day_first, month_first) {
        (Some(d), Some(m)) if d == m => Some(d),
        (Some(d), None) => Some(d),
        (None, Some(m)) => Some(m),
        _ => None,
    }
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
    let Some(stem) = stem(param) else {
        return Vec::new();
    };
    candidates
        .filter(|name| name.rsplit('.').next().is_some_and(|n| n.contains(stem)))
        .map(str::to_owned)
        .collect()
}

/// A parameter's word stem: `category_id` → `categor`, `account_ids` →
/// `account`, `date` → `date`. `None` below 3 characters.
pub fn stem(param: &str) -> Option<&str> {
    let s = param
        .strip_suffix("_ids")
        .or_else(|| param.strip_suffix("_id"))
        .unwrap_or(param);
    let s = s.strip_suffix(['y', 's']).unwrap_or(s);
    (s.len() >= 3).then_some(s)
}

/// The id of the one entry a lookup returned, when it is valid for the
/// parameter (SPEC §11 #26). The entries are the result itself or its only
/// array field.
pub fn single_id(result: &Value, param_schema: &Value) -> Option<String> {
    let entries = match result {
        Value::Array(a) => a,
        Value::Object(o) => {
            let mut arrays = o.values().filter_map(Value::as_array);
            match (arrays.next(), arrays.next()) {
                (Some(a), None) => a,
                _ => return None,
            }
        }
        _ => return None,
    };
    let [entry] = entries.as_slice() else {
        return None;
    };
    let id = entry["id"].as_str()?;
    let value = Value::String(id.to_owned());
    (!is_empty(&value, param_schema) && !breaks_format(&value, param_schema)).then(|| id.to_owned())
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
    fn one_date_is_found_in_attachment_text() {
        let d = |y, m, day| chrono::NaiveDate::from_ymd_opt(y, m, day);
        let bill = "PIZZA NAPOLI\nDate: 2026-10-19\n1 Soda Q 15.00\nTOTAL Q 110.00";
        assert_eq!(attachment_date(&[bill]), d(2026, 10, 19));
        assert_eq!(attachment_date(&[bill, "on 2026-10-19."]), d(2026, 10, 19));
        assert_eq!(attachment_date(&[bill, "due 2026-11-01"]), None);
        assert_eq!(attachment_date(&["Fecha 19/10/2026"]), d(2026, 10, 19));
        assert_eq!(attachment_date(&["05/04/2026"]), None);
        assert_eq!(attachment_date(&["05.05.2026"]), d(2026, 5, 5));
        assert_eq!(attachment_date(&["TOTAL Q 110.00", "2026/10"]), None);
    }

    #[test]
    fn a_tool_has_a_date_param_only_when_it_has_one() {
        let one = json!({"properties": {"date": {"type": "string"}, "amount": {}}});
        assert_eq!(date_param(&one).as_deref(), Some("date"));
        let fmt = json!({"properties": {"when": {"type": "string", "format": "date"}}});
        assert_eq!(date_param(&fmt).as_deref(), Some("when"));
        let range = json!({"properties": {"start_date": {"type": "string"},
                                          "end_date": {"type": "string"}}});
        assert_eq!(date_param(&range), None);
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
    fn carry_over_fills_only_what_the_new_call_leaves_out() {
        let prev = json!({"amount": 250, "date": "2026-10-16", "notes": "dinner"});
        let mut args = json!({"amount": 300, "account_id": "a1", "notes": ""});
        let filled = carry_over(&prev, &mut args, &params());
        assert_eq!(filled, vec!["date", "notes"]);
        assert_eq!(
            args,
            json!({"amount": 300, "account_id": "a1", "date": "2026-10-16", "notes": "dinner"})
        );
        let before = args.clone();
        assert!(carry_over(&json!({}), &mut args, &params()).is_empty());
        assert_eq!(args, before);
    }

    #[test]
    fn single_id_takes_the_only_valid_entry() {
        let id = "d65cf7f5-6fcc-4ba1-a6e6-69fa850c4444";
        let uuid = json!({"type": "string", "format": "uuid"});
        let one = json!({"items": [{"id": id, "name": "Main"}], "total": 1});
        assert_eq!(single_id(&one, &uuid).as_deref(), Some(id));
        assert_eq!(
            single_id(&json!([{"id": "a1"}]), &json!({})).as_deref(),
            Some("a1")
        );
        let two = json!({"items": [{"id": id}, {"id": id}], "total": 2});
        assert_eq!(single_id(&two, &uuid), None);
        assert_eq!(single_id(&json!([{"id": "main"}]), &uuid), None, "format");
        assert_eq!(single_id(&json!({"total": 1}), &uuid), None);
        assert_eq!(single_id(&json!({"a": [], "b": []}), &uuid), None);
    }

    #[test]
    fn stem_strips_id_suffixes_and_plurals() {
        assert_eq!(stem("category_id"), Some("categor"));
        assert_eq!(stem("account_ids"), Some("account"));
        assert_eq!(stem("date"), Some("date"));
        assert_eq!(stem("id"), None);
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
