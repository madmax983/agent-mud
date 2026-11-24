//! `View`: read-only structured data for scripts.
//!
//! A view is a JSON tree and a pointer into it. Cloning one is O(1), so a
//! script that passes `ctx` around costs nothing per call, where a plain
//! Rhai map would be copied each time. No setter is registered: a script
//! cannot write to a view. Reading a missing key faults, so a dry run
//! catches typos. Views never hold player text.

use std::sync::Arc;

use rhai::{Dynamic, Engine};
use serde_json::Value;

use crate::sandbox::{RhaiResult, fault};

/// A read-only view of a JSON tree.
#[derive(Debug, Clone)]
pub struct View {
    root: Arc<Value>,
    ptr: Arc<str>,
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

impl View {
    /// A view of the whole of `v`.
    #[must_use]
    pub fn new(v: Value) -> Self {
        Self {
            root: Arc::new(v),
            ptr: Arc::from(""),
        }
    }

    /// The value this view points at.
    #[must_use]
    pub fn value(&self) -> &Value {
        self.root.pointer(&self.ptr).unwrap_or(&Value::Null)
    }

    fn child(&self, step: &str) -> Self {
        Self {
            root: Arc::clone(&self.root),
            ptr: Arc::from(format!("{}/{step}", self.ptr)),
        }
    }

    /// Converts the value at `step` below this view: a leaf to its Rhai
    /// value, an object or array to a sub-view.
    fn read(&self, step: &str, v: &Value) -> RhaiResult<Dynamic> {
        Ok(match v {
            Value::Null => Dynamic::UNIT,
            Value::Bool(b) => Dynamic::from_bool(*b),
            Value::Number(n) => Dynamic::from_int(
                n.as_i64()
                    .ok_or_else(|| fault("a view holds a number scripts cannot read"))?,
            ),
            Value::String(s) => Dynamic::from(s.clone()),
            Value::Array(_) | Value::Object(_) => Dynamic::from(self.child(step)),
        })
    }

    /// `v.key` and `v["key"]`.
    ///
    /// # Errors
    /// The key is missing, or this is not an object.
    pub fn get(&self, key: &str) -> RhaiResult<Dynamic> {
        self.value()
            .as_object()
            .and_then(|m| m.get(key))
            .map_or_else(
                || Err(fault(format!("no property {key:?}"))),
                |v| self.read(&escape(key), v),
            )
    }

    /// `v[i]`.
    ///
    /// # Errors
    /// The index is out of range, or this is not an array.
    pub fn at(&self, i: i64) -> RhaiResult<Dynamic> {
        let list = self
            .value()
            .as_array()
            .ok_or_else(|| fault("this view is not a list"))?;
        let at = usize::try_from(i)
            .ok()
            .filter(|n| *n < list.len())
            .ok_or_else(|| fault(format!("index {i} is out of range (length {})", list.len())))?;
        self.read(&at.to_string(), &list[at])
    }

    /// The number of entries of an array or an object; 0 otherwise.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.value() {
            Value::Array(a) => a.len(),
            Value::Object(m) => m.len(),
            _ => 0,
        }
    }

    /// Whether it has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether an object has `key`.
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.value()
            .as_object()
            .is_some_and(|m| m.contains_key(key))
    }

    fn items(&self) -> Vec<Dynamic> {
        match self.value() {
            Value::Array(a) => (0..a.len())
                .filter_map(|i| self.read(&i.to_string(), &a[i]).ok())
                .collect(),
            Value::Object(m) => m.keys().map(|k| Dynamic::from(k.clone())).collect(),
            _ => Vec::new(),
        }
    }
}

impl IntoIterator for View {
    type Item = Dynamic;
    type IntoIter = std::vec::IntoIter<Dynamic>;

    /// The elements of a list, or the keys of an object.
    fn into_iter(self) -> Self::IntoIter {
        self.items().into_iter()
    }
}

/// Registers `View`: property and index reads, `in`, `len`, iteration.
pub fn register(engine: &mut Engine) {
    engine.register_type_with_name::<View>("View");
    engine.register_indexer_get(|v: &mut View, k: &str| v.get(k));
    engine.register_indexer_get(|v: &mut View, i: i64| v.at(i));
    engine.register_fn("contains", |v: &mut View, k: &str| v.has(k));
    engine.register_fn("len", |v: &mut View| {
        i64::try_from(v.len()).unwrap_or(i64::MAX)
    });
    engine.register_fn("is_empty", |v: &mut View| v.is_empty());
    engine.register_iterator::<View>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_view_reads_leaves_and_sub_views() {
        let v = View::new(json!({ "a/b": { "c": [1, true, "x", null] }, "n": 2 }));
        let inner = v.get("a/b").unwrap().cast::<View>();
        let list = inner.get("c").unwrap().cast::<View>();
        assert_eq!(list.len(), 4);
        assert_eq!(list.at(0).unwrap().as_int(), Ok(1));
        assert_eq!(list.at(1).unwrap().as_bool(), Ok(true));
        assert!(list.at(3).unwrap().is_unit());
        assert!(list.at(4).is_err());
        assert!(list.at(-1).is_err());
        assert!(
            v.get("zz")
                .unwrap_err()
                .to_string()
                .contains("no property \"zz\"")
        );
        assert!(v.has("n") && !v.has("m"));
        assert!(v.has("n"));
        assert_eq!(v.into_iter().count(), 2);
        assert!(View::new(json!(1.5)).value().is_f64());
        assert!(View::new(json!({ "f": 1.5 })).get("f").is_err());
        assert!(View::new(json!(3)).is_empty());
    }
}
