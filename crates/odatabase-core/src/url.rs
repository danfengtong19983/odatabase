//! Links and context URLs.

use odatabase_edm::{Model, Value};
use odatabase_sql::Record;
use odatabase_uri::{ExpandItem, QueryOptions, SelectItem};

/// The canonical key predicate of `rec`, such as `('a')` or `(Code=1,Id='a')`.
pub fn key_predicate(model: &Model, type_name: &str, rec: &Record) -> Option<String> {
    let key = model.key_of(type_name);
    let values: Vec<(&String, &Value)> = key
        .iter()
        .map(|k| Some((k, rec.value(k)?)))
        .collect::<Option<_>>()?;
    if let [(_, v)] = values.as_slice() {
        return Some(format!("({})", encode(&v.to_uri_literal())));
    }
    let parts: Vec<String> = values
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(&v.to_uri_literal())))
        .collect();
    Some(format!("({})", parts.join(",")))
}

/// Escapes the characters that would break a path segment.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(b as char),
            b'-' | b'.' | b'_' | b'~' | b'\'' | b':' | b'+' | b',' | b'=' | b'(' | b')' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The entity set containing instances of `type_name`, looking through base types.
pub fn set_of<'m>(model: &'m Model, type_name: &str) -> Option<&'m str> {
    model
        .type_chain(type_name)
        .into_iter()
        .find_map(|t| model.entity_set_of_type(&t.name))
        .map(|s| s.name.as_str())
}

/// The set of `type_name` in a context URL, with a cast when the set holds a base type.
pub fn set_path(model: &Model, type_name: &str) -> Option<String> {
    let set = set_of(model, type_name)?;
    match model.entity_sets.get(set) {
        Some(s) if s.entity_type != type_name => {
            Some(format!("{set}/{}", model.qualified(type_name)))
        }
        _ => Some(set.to_owned()),
    }
}

/// The canonical URL of an entity, relative to the service root.
pub fn entity_id(model: &Model, type_name: &str, rec: &Record) -> Option<String> {
    if let Some(s) = model
        .singletons
        .values()
        .find(|s| s.entity_type == type_name)
    {
        return Some(s.name.clone());
    }
    Some(format!(
        "{}{}",
        set_of(model, type_name)?,
        key_predicate(model, type_name, rec)?
    ))
}

/// `(Id,Name,Roles(Code))`, the select list of a context URL.
pub fn select_list(options: &QueryOptions) -> String {
    let mut items: Vec<String> = Vec::new();
    if let Some(select) = &options.select {
        for item in select {
            match item {
                SelectItem::Star => items.push("*".to_owned()),
                SelectItem::Path(p) => items.push(p.join("/")),
            }
        }
    }
    for e in &options.expand {
        items.push(expand_entry(e));
    }
    if items.is_empty() {
        String::new()
    } else {
        format!("({})", items.join(","))
    }
}

fn expand_entry(e: &ExpandItem) -> String {
    let name = if e.prefix.is_empty() {
        e.navigation.clone()
    } else {
        format!("{}/{}", e.prefix.join("/"), e.navigation)
    };
    let inner = select_list(&e.options);
    if inner.is_empty() {
        format!("{name}()")
    } else {
        format!("{name}{inner}")
    }
}

/// The query string of a next link: the original options with a new `$skiptoken`.
pub fn next_query(query: &str, token: &str) -> String {
    let mut parts: Vec<String> = query
        .split('&')
        .filter(|p| {
            !p.is_empty() && !p.starts_with("$skiptoken=") && !p.starts_with("%24skiptoken=")
        })
        .map(str::to_owned)
        .collect();
    parts.push(format!("$skiptoken={}", encode(token)));
    parts.join("&")
}
