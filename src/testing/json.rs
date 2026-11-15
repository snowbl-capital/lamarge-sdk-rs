//! JSON fixtures: `src/model/fixtures/<file>.json`, or the part of one a JSON pointer names
//! (`"book/levels/0"`).

use serde_json::Value;

/// The fixture `name`: a file, then an optional JSON pointer into it.
pub(crate) fn fixture(name: &str) -> Value {
    let (file, pointer) = name
        .split_once('/')
        .map_or((name, String::new()), |(file, pointer)| (file, format!("/{pointer}")));
    let path = format!("{}/src/model/fixtures/{file}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    let value: Value =
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{path}: {error}"));
    value.pointer(&pointer).unwrap_or_else(|| panic!("{name}: no such part")).clone()
}

/// A list answer holding the fixture `name` once.
pub(crate) fn list(name: &str) -> Value {
    Value::Array(vec![fixture(name)])
}

/// A history page holding `items`, followed by `next` when given.
pub(crate) fn page(items: &[Value], next: Option<&str>) -> Value {
    serde_json::json!({"data": items, "next_cursor": next, "has_more": next.is_some()})
}
