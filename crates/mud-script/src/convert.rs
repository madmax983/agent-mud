//! Values across the script boundary.

use rhai::{Array, Dynamic, Map};
use serde_json::Value;

use crate::view::View;

/// The longest string mind memory may hold.
pub const MEMORY_STRING_MAX: usize = 200;
/// The most entries of one list or map in mind memory.
pub const MEMORY_ENTRIES_MAX: usize = 64;
/// The longest map key in mind memory.
pub const MEMORY_KEY_MAX: usize = 64;
/// The deepest nesting of mind memory (the top map is depth 1).
pub const MEMORY_DEPTH_MAX: usize = 3;
/// The largest mind memory, as JSON.
pub const MEMORY_JSON_MAX: usize = 8_192;

/// A read-only view of `v`.
#[must_use]
pub fn to_view(v: Value) -> View {
    View::new(v)
}

/// A plain Rhai value of `v` (maps and arrays are copies a script may
/// change).
///
/// # Errors
/// `v` holds a number that is not an integer.
pub fn to_dynamic(v: &Value) -> Result<Dynamic, String> {
    Ok(match v {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from_bool(*b),
        Value::Number(n) => Dynamic::from_int(
            n.as_i64()
                .ok_or_else(|| format!("{n} is not an integer; scripts use integers"))?,
        ),
        Value::String(s) => Dynamic::from(s.clone()),
        Value::Array(a) => {
            let out: Result<Array, String> = a.iter().map(to_dynamic).collect();
            Dynamic::from_array(out?)
        }
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                out.insert(k.as_str().into(), to_dynamic(x)?);
            }
            Dynamic::from_map(out)
        }
    })
}

fn kind_of(d: &Dynamic) -> String {
    d.type_name()
        .rsplit("::")
        .next()
        .unwrap_or("value")
        .to_owned()
}

fn plain(d: &Dynamic, path: &str, depth: usize) -> Result<Value, String> {
    let bad = |why: &str| format!("memory may hold only plain data ({why} at {path})");
    if d.is_unit() {
        return Ok(Value::Null);
    }
    if let Ok(b) = d.as_bool() {
        return Ok(Value::Bool(b));
    }
    if let Ok(n) = d.as_int() {
        return Ok(Value::from(n));
    }
    if d.is_string() {
        let s = d.clone().into_string().unwrap_or_default();
        if s.chars().count() > MEMORY_STRING_MAX {
            return Err(bad(&format!(
                "a string over {MEMORY_STRING_MAX} characters"
            )));
        }
        return Ok(Value::String(s));
    }
    if depth > MEMORY_DEPTH_MAX && (d.is_array() || d.is_map()) {
        return Err(bad(&format!("nesting deeper than {MEMORY_DEPTH_MAX}")));
    }
    if d.is_array() {
        let a = d.clone().into_array().map_err(|_| bad("an array"))?;
        if a.len() > MEMORY_ENTRIES_MAX {
            return Err(bad(&format!("a list over {MEMORY_ENTRIES_MAX} entries")));
        }
        let out: Result<Vec<Value>, String> = a
            .iter()
            .enumerate()
            .map(|(i, x)| plain(x, &format!("{path}[{i}]"), depth + 1))
            .collect();
        return Ok(Value::Array(out?));
    }
    if d.is_map() {
        let m = d.clone().try_cast::<Map>().ok_or_else(|| bad("a map"))?;
        if m.len() > MEMORY_ENTRIES_MAX {
            return Err(bad(&format!("a map over {MEMORY_ENTRIES_MAX} entries")));
        }
        let mut out = serde_json::Map::new();
        for (k, x) in &m {
            if k.chars().count() > MEMORY_KEY_MAX {
                return Err(bad(&format!("a key over {MEMORY_KEY_MAX} characters")));
            }
            out.insert(k.to_string(), plain(x, &format!("{path}.{k}"), depth + 1)?);
        }
        return Ok(Value::Object(out));
    }
    Err(bad(&format!("found {}", kind_of(d))))
}

/// Mind memory from a script value: only `()`, bools, integers, strings,
/// lists and maps, within the size limits.
///
/// # Errors
/// Anything else, or a limit is passed; the message names the path.
pub fn from_dynamic(d: &Dynamic) -> Result<Value, String> {
    if !d.is_map() {
        return Err(format!("memory must be a map, not {}", kind_of(d)));
    }
    let v = plain(d, "memory", 1)?;
    let size = serde_json::to_string(&v).map_or(usize::MAX, |s| s.len());
    if size > MEMORY_JSON_MAX {
        return Err(format!(
            "memory is {size} bytes as JSON; the limit is {MEMORY_JSON_MAX}"
        ));
    }
    Ok(v)
}

/// A binding's `config` table as JSON.
///
/// # Errors
/// It holds a float or a date; the message names the key.
pub fn config_json(table: &toml::Table) -> Result<Value, String> {
    fn walk(v: &toml::Value, key: &str) -> Result<Value, String> {
        Ok(match v {
            toml::Value::String(s) => Value::String(s.clone()),
            toml::Value::Integer(n) => Value::from(*n),
            toml::Value::Boolean(b) => Value::Bool(*b),
            toml::Value::Float(_) | toml::Value::Datetime(_) => return Err(key.to_owned()),
            toml::Value::Array(a) => Value::Array(
                a.iter()
                    .enumerate()
                    .map(|(i, x)| walk(x, &format!("{key}[{i}]")))
                    .collect::<Result<_, _>>()?,
            ),
            toml::Value::Table(t) => {
                let mut m = serde_json::Map::new();
                for (k, x) in t {
                    let at = if key.is_empty() {
                        k.clone()
                    } else {
                        format!("{key}.{k}")
                    };
                    m.insert(k.clone(), walk(x, &at)?);
                }
                Value::Object(m)
            }
        })
    }
    walk(&toml::Value::Table(table.clone()), "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_values_round_trip_and_others_are_refused() {
        let v = json!({ "day": 3, "warned": { "pc_ada": 1 }, "s": "x", "l": [true, null] });
        let d = to_dynamic(&v).unwrap();
        assert_eq!(from_dynamic(&d).unwrap(), v);
        assert!(to_dynamic(&json!(1.5)).is_err());
        let deep = to_dynamic(&json!({ "a": { "b": { "c": { "d": 1 } } } })).unwrap();
        let ok = to_dynamic(&json!({ "a": { "b": { "c": 1 } } })).unwrap();
        assert!(from_dynamic(&ok).is_ok());
        assert!(
            from_dynamic(&deep)
                .unwrap_err()
                .contains("nesting deeper than 3")
        );
        let long = to_dynamic(&json!({ "s": "x".repeat(201) })).unwrap();
        assert!(from_dynamic(&long).unwrap_err().contains("at memory.s"));
        let view = Dynamic::from(View::new(json!({})));
        let mut m = Map::new();
        m.insert("v".into(), view);
        let err = from_dynamic(&Dynamic::from_map(m)).unwrap_err();
        assert!(err.contains("found View"), "{err}");
        assert!(from_dynamic(&Dynamic::from_int(1)).is_err());
        let big: serde_json::Map<String, Value> = (0..60)
            .map(|i| (format!("k{i}"), json!("y".repeat(190))))
            .collect();
        let err = from_dynamic(&to_dynamic(&Value::Object(big)).unwrap()).unwrap_err();
        assert!(err.contains("the limit is 8192"), "{err}");
    }

    #[test]
    fn config_refuses_floats_and_dates_by_key() {
        let t: toml::Table = toml::from_str("a = 1\nb = { c = [1, 2.5] }").unwrap();
        assert_eq!(config_json(&t).unwrap_err(), "b.c[1]");
        let t: toml::Table = toml::from_str("d = 1979-05-27").unwrap();
        assert_eq!(config_json(&t).unwrap_err(), "d");
        let t: toml::Table = toml::from_str("n = 20\ns = \"x\"\nok = true").unwrap();
        assert_eq!(
            config_json(&t).unwrap(),
            json!({ "n": 20, "s": "x", "ok": true })
        );
    }
}
