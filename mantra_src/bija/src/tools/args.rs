//! Tool-call arguments shaped to the tool's schema before dispatch (SPEC §6.1).
//!
//! Models get the JSON right and a type wrong often enough to matter: a nested array sent as a
//! JSON string (`"phases": "[{…}]"`), `"30"` for 30, `"true"` for true. Refusing that costs a round
//! trip, and a model that made the mistake once tends to make it again every round, so a turn can
//! loop on it. `coerce` walks the tool's schema beside the value and repairs exactly those cases,
//! and only where the schema names the one type it wants. Anything it cannot read a single answer
//! from — no schema, a `$ref`, branches that disagree, a string that is not JSON of the wanted type
//! — stays as the model sent it, so the tool's own validation still speaks.

use serde_json::Value;

/// Nesting followed, in schema and value alike. serde_json already caps parsed depth at 128; this
/// keeps a schema built by hand (or a self-similar one) from costing more than it is worth.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Object,
    Array,
    Integer,
    Number,
    Boolean,
    String,
}

/// Rewrites `value` in place where `schema` asks for a different JSON type than the model sent and
/// the value converts losslessly (see the module note). Never fails: a schema it does not
/// understand leaves the value as it is.
pub fn coerce(schema: &Value, value: &mut Value) {
    walk(&[schema], value, 0);
}

/// `cands`: every schema the value must satisfy at this point — one, or the branches of an
/// `anyOf`/`oneOf` that define this property. A type is applied only when they all agree.
fn walk(cands: &[&Value], value: &mut Value, depth: usize) {
    if depth > MAX_DEPTH || cands.is_empty() {
        return;
    }
    if let Some(kind) = agreed_kind(cands, depth) {
        convert(kind, value);
    }
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                let subs = collect(cands, depth, |s| s.get("properties").and_then(|p| p.get(key.as_str())));
                walk(&subs, v, depth + 1);
            }
        }
        Value::Array(items) => {
            // Only the one-schema-for-every-item form; tuple-form `items` is left alone.
            let subs = collect(cands, depth, |s| s.get("items").filter(|i| i.is_object()));
            for v in items.iter_mut() {
                walk(&subs, v, depth + 1);
            }
        }
        _ => {}
    }
}

/// The subschemas `pick` finds in each candidate and in its `anyOf`/`oneOf` branches.
fn collect<'a>(cands: &[&'a Value], depth: usize, pick: impl Fn(&'a Value) -> Option<&'a Value>) -> Vec<&'a Value> {
    let mut all = Vec::new();
    for c in cands {
        expand(c, depth, &mut all);
    }
    all.into_iter().filter_map(pick).filter(|s| s.is_object()).collect()
}

/// `s` and, recursively, the branches of its `anyOf`/`oneOf`.
fn expand<'a>(s: &'a Value, depth: usize, out: &mut Vec<&'a Value>) {
    if depth > MAX_DEPTH {
        return;
    }
    out.push(s);
    for b in branches(s) {
        expand(b, depth + 1, out);
    }
}

fn branches(s: &Value) -> impl Iterator<Item = &Value> {
    ["anyOf", "oneOf"].into_iter().filter_map(move |k| s.get(k).and_then(Value::as_array)).flatten()
}

fn agreed_kind(cands: &[&Value], depth: usize) -> Option<Kind> {
    let mut kinds = cands.iter().map(|s| kind_of(s, depth));
    let first = kinds.next()??;
    kinds.all(|k| k == Some(first)).then_some(first)
}

/// The one type a schema asks for: its own `type` (a list counts when one entry besides `null`
/// remains), else the type every `anyOf`/`oneOf` branch agrees on.
fn kind_of(s: &Value, depth: usize) -> Option<Kind> {
    if depth > MAX_DEPTH {
        return None;
    }
    match s.get("type") {
        Some(Value::String(t)) => return kind_named(t),
        Some(Value::Array(ts)) => {
            let mut named = ts.iter().filter(|t| t.as_str() != Some("null"));
            return match (named.next(), named.next()) {
                (Some(Value::String(t)), None) => kind_named(t),
                _ => None,
            };
        }
        Some(_) => return None,
        None => {}
    }
    let mut kinds = branches(s).map(|b| kind_of(b, depth + 1));
    let first = kinds.next()??;
    kinds.all(|k| k == Some(first)).then_some(first)
}

fn kind_named(t: &str) -> Option<Kind> {
    Some(match t {
        "object" => Kind::Object,
        "array" => Kind::Array,
        "integer" => Kind::Integer,
        "number" => Kind::Number,
        "boolean" => Kind::Boolean,
        "string" => Kind::String,
        _ => return None,
    })
}

fn convert(kind: Kind, value: &mut Value) {
    let new = match (kind, &*value) {
        (Kind::Object, Value::String(s)) => serde_json::from_str::<Value>(s).ok().filter(Value::is_object),
        (Kind::Array, Value::String(s)) => serde_json::from_str::<Value>(s).ok().filter(Value::is_array),
        // JSON's own number grammar: no `inf`, hex or trailing text; `"30.0"` is no integer.
        (Kind::Integer, Value::String(s)) => serde_json::from_str::<Value>(s).ok().filter(|v| v.is_i64() || v.is_u64()),
        (Kind::Number, Value::String(s)) => serde_json::from_str::<Value>(s).ok().filter(Value::is_number),
        (Kind::Boolean, Value::String(s)) => match s.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        (Kind::String, Value::Number(n)) => Some(Value::String(n.to_string())),
        (Kind::String, Value::Bool(b)) => Some(Value::String(b.to_string())),
        _ => None,
    };
    if let Some(v) = new {
        *value = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn coerced(schema: Value, mut args: Value) -> Value {
        coerce(&schema, &mut args);
        args
    }

    fn plan_schema() -> Value {
        json!({"type": "object", "properties": {"plan": {"type": "object", "properties": {
            "title": {"type": "string"},
            "phases": {"type": "array", "items": {"type": "object", "properties": {
                "gate": {"type": "object", "properties": {"checks": {"type": "array", "items": {"type": "string"}}}},
                "retries": {"type": "integer"}
            }}}
        }}}})
    }

    #[test]
    fn a_nested_array_sent_as_a_string_is_parsed_and_walked_into() {
        let args = json!({"plan": {"title": "T", "phases": r#"[{"gate": {"checks": "[\"cargo test\"]"}, "retries": "2"}]"#}});
        let want = json!({"plan": {"title": "T", "phases": [{"gate": {"checks": ["cargo test"]}, "retries": 2}]}});
        assert_eq!(coerced(plan_schema(), args), want);
    }

    #[test]
    fn a_nested_object_sent_as_a_string_is_parsed() {
        let args = json!({"plan": r#"{"title": "T", "phases": []}"#});
        assert_eq!(coerced(plan_schema(), args), json!({"plan": {"title": "T", "phases": []}}));
    }

    #[test]
    fn scalars_follow_the_schema() {
        let schema = json!({"type": "object", "properties": {
            "timeout_secs": {"type": "integer"}, "ratio": {"type": "number"}, "replace_all": {"type": "boolean"},
            "word": {"type": "string"}, "flag": {"type": "string"}
        }});
        let args = json!({"timeout_secs": "30", "ratio": " 0.5 ", "replace_all": "true", "word": 7, "flag": false});
        assert_eq!(coerced(schema, args), json!({"timeout_secs": 30, "ratio": 0.5, "replace_all": true, "word": "7", "flag": "false"}));
    }

    #[test]
    fn what_cannot_convert_exactly_stays_as_sent() {
        let schema = json!({"type": "object", "properties": {
            "n": {"type": "integer"}, "m": {"type": "integer"}, "x": {"type": "number"}, "b": {"type": "boolean"},
            "list": {"type": "array"}, "obj": {"type": "object"}, "prose": {"type": "array"}
        }});
        let args = json!({"n": "30.0", "m": "thirty", "x": "inf", "b": "yes", "list": "{\"a\": 1}", "obj": "[1]", "prose": "not json"});
        assert_eq!(coerced(schema, args.clone()), args, "wrong-typed JSON and non-JSON strings are untouched");
    }

    #[test]
    fn without_a_schema_or_with_matching_types_nothing_changes() {
        let args = json!({"plan": {"phases": "[]"}, "n": "3"});
        assert_eq!(coerced(Value::Null, args.clone()), args);
        assert_eq!(coerced(json!({}), args.clone()), args);
        assert_eq!(coerced(json!({"type": "object", "properties": {}}), args.clone()), args, "unknown properties are not guessed at");
        let typed = json!({"plan": {"title": "T", "phases": [{"retries": 1}]}});
        assert_eq!(coerced(plan_schema(), typed.clone()), typed);
    }

    #[test]
    fn items_apply_to_every_element_and_depth_is_followed() {
        let schema = json!({"type": "array", "items": {"type": "array", "items": {"type": "object", "properties": {"n": {"type": "integer"}}}}});
        let args = json!(["[{\"n\": \"1\"}]", [{"n": "2"}, {"n": 3}]]);
        assert_eq!(coerced(schema, args), json!([[{"n": 1}], [{"n": 2}, {"n": 3}]]));
    }

    #[test]
    fn branches_count_only_when_they_agree() {
        let schema = json!({"type": "object", "properties": {
            "agree": {"anyOf": [{"type": "integer", "minimum": 0}, {"type": "integer", "maximum": -10}]},
            "differ": {"oneOf": [{"type": "integer"}, {"type": "string"}]},
            "nullable": {"type": ["integer", "null"]},
            "either": {"type": ["integer", "string"]}
        }});
        let args = json!({"agree": "5", "differ": "5", "nullable": "5", "either": "5"});
        assert_eq!(coerced(schema, args), json!({"agree": 5, "differ": "5", "nullable": 5, "either": "5"}));
    }

    #[test]
    fn a_weird_schema_never_panics() {
        let weird = [json!({"type": 7}), json!({"properties": "nope"}), json!({"items": [1, 2]}), json!({"anyOf": "x"}), json!({"anyOf": []}), json!({"$ref": "#"}), json!([1, 2]), json!("object")];
        for schema in weird {
            let args = json!({"a": "1", "b": ["[1]"]});
            assert_eq!(coerced(schema.clone(), args.clone()), args, "{schema}");
        }
        // Deeper than the cap: walked as far as it goes, no stack overflow.
        let mut schema = json!({"type": "integer"});
        let mut args = json!("1");
        for _ in 0..200 {
            schema = json!({"type": "array", "items": schema});
            args = json!([args]);
        }
        coerce(&schema, &mut args);
    }
}
