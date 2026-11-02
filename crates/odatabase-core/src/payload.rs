//! Reads OData JSON request bodies against the model.

use crate::ODataError;
use odatabase_edm::{Model, PrimitiveKind, Property, TypeKind, Value};
use serde_json::{Map, Value as Json};

/// A property value from a request body.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyValue {
    Value(Value),
    Complex(Option<Vec<(String, PropertyValue)>>),
    Collection(Vec<PropertyValue>),
}

/// An entity from a request body, which may hold nested entities and links.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityPayload {
    pub type_name: String,
    pub values: Vec<(String, PropertyValue)>,
    /// Related entities given inline. Empty for a single-valued navigation set to null.
    pub nested: Vec<(String, Vec<EntityPayload>)>,
    /// `Nav@odata.bind`: navigation name and the URLs of existing entities.
    pub binds: Vec<(String, Vec<String>)>,
    /// `Nav@delta`: changes to some members of a collection navigation.
    pub deltas: Vec<(String, Vec<DeltaItem>)>,
    /// `@odata.etag` of a nested entity, checked before it is updated.
    pub etag: Option<String>,
}

/// One entry of a delta payload.
#[derive(Clone, Debug, PartialEq)]
pub enum DeltaItem {
    /// An entity to create or update. `id` names it when the properties don't.
    Entity {
        id: Option<String>,
        entity: EntityPayload,
    },
    /// An existing entity to relate, given only by its URL.
    Link(String),
    /// An entity to take away, named by `id` or by its key properties. With `deleted` it stops
    /// existing, otherwise only the relationship ends.
    Removed {
        id: Option<String>,
        entity: EntityPayload,
        deleted: bool,
    },
}

impl EntityPayload {
    pub fn value(&self, property: &str) -> Option<&PropertyValue> {
        self.values
            .iter()
            .find(|(n, _)| n == property)
            .map(|(_, v)| v)
    }
}

/// Parses a body as an entity of `declared` or of the derived type it names.
pub fn parse_entity(
    model: &Model,
    declared: &str,
    body: &[u8],
) -> Result<EntityPayload, ODataError> {
    let doc: Json = serde_json::from_slice(body)
        .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
    let Json::Object(obj) = doc else {
        return Err(ODataError::bad_request("expected a JSON object"));
    };
    read_entity(model, declared, &obj, 0)
}

/// Parses the body of `PATCH` on a collection: a delta payload for entities of `declared`.
pub fn parse_delta(
    model: &Model,
    declared: &str,
    body: &[u8],
) -> Result<Vec<DeltaItem>, ODataError> {
    let doc: Json = serde_json::from_slice(body)
        .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
    let items = doc
        .get("value")
        .and_then(Json::as_array)
        .ok_or_else(|| ODataError::bad_request("expected a delta payload with a value array"))?;
    read_delta(model, declared, items, 0)
}

fn read_delta(
    model: &Model,
    declared: &str,
    items: &[Json],
    depth: u32,
) -> Result<Vec<DeltaItem>, ODataError> {
    items
        .iter()
        .map(|item| {
            let obj = item
                .as_object()
                .ok_or_else(|| ODataError::bad_request("a delta entry must be an object"))?;
            read_delta_item(model, declared, obj, depth)
        })
        .collect()
}

fn read_delta_item(
    model: &Model,
    declared: &str,
    obj: &Map<String, Json>,
    depth: u32,
) -> Result<DeltaItem, ODataError> {
    let text = |name: &str| obj.get(name).and_then(Json::as_str).map(str::to_owned);
    let removed = obj.get("@removed").or(obj.get("@odata.removed"));
    let context = text("@context").or(text("@odata.context"));
    let tombstone = context.is_some_and(|c| c.ends_with("$deletedEntity"));
    let mut id = text("@id").or(text("@odata.id"));
    if removed.is_none() && !tombstone {
        if let (Some(url), true) = (&id, obj.keys().all(|k| k.contains('@'))) {
            return Ok(DeltaItem::Link(url.clone()));
        }
        let entity = read_entity(model, declared, obj, depth)?;
        return Ok(DeltaItem::Entity { id, entity });
    }
    let reason = removed
        .and_then(|r| r.get("reason"))
        .or(obj.get("reason"))
        .map(|r| r.as_str().unwrap_or_default());
    let deleted = match reason {
        Some("deleted") => true,
        Some("changed") | None => false,
        Some(other) => {
            return Err(ODataError::bad_request(format!("invalid reason {other}")));
        }
    };
    let mut rest = obj.clone();
    if tombstone {
        id = id.or(text("id"));
        rest.remove("id");
        rest.remove("reason");
    }
    let entity = read_entity(model, declared, &rest, depth)?;
    Ok(DeltaItem::Removed {
        id,
        entity,
        deleted,
    })
}

const MAX_NESTING: u32 = 16;

fn read_entity(
    model: &Model,
    declared: &str,
    obj: &Map<String, Json>,
    depth: u32,
) -> Result<EntityPayload, ODataError> {
    if depth > MAX_NESTING {
        return Err(ODataError::bad_request("the payload is nested too deeply"));
    }
    let type_name = concrete_type(model, declared, obj)?;
    let mut out = EntityPayload {
        type_name: type_name.clone(),
        values: Vec::new(),
        nested: Vec::new(),
        binds: Vec::new(),
        deltas: Vec::new(),
        etag: obj
            .get("@odata.etag")
            .or(obj.get("@etag"))
            .and_then(Json::as_str)
            .map(str::to_owned),
    };
    for (name, json) in obj {
        if let Some(nav) = name
            .strip_suffix("@odata.bind")
            .or(name.strip_suffix("@bind"))
        {
            let n = model
                .navigation(&type_name, nav)
                .ok_or_else(|| ODataError::bad_request(format!("unknown navigation {nav}")))?;
            let urls = match (json, n.collection) {
                (Json::String(u), false) => vec![u.clone()],
                (Json::Array(items), true) => items
                    .iter()
                    .map(|i| {
                        i.as_str().map(str::to_owned).ok_or_else(|| {
                            ODataError::bad_request(format!("{name} must hold URLs"))
                        })
                    })
                    .collect::<Result<_, _>>()?,
                _ => return Err(ODataError::bad_request(format!("invalid {name}"))),
            };
            out.binds.push((nav.to_owned(), urls));
            continue;
        }
        if let Some(nav) = name
            .strip_suffix("@odata.delta")
            .or(name.strip_suffix("@delta"))
        {
            let n = model
                .navigation(&type_name, nav)
                .ok_or_else(|| ODataError::bad_request(format!("unknown navigation {nav}")))?;
            if !n.collection {
                return Err(ODataError::bad_request(format!("{nav} is single-valued")));
            }
            let Json::Array(items) = json else {
                return Err(ODataError::bad_request(format!("{name} must be an array")));
            };
            let items = read_delta(model, &n.target, items, depth + 1)?;
            out.deltas.push((nav.to_owned(), items));
            continue;
        }
        if name.contains('@') {
            continue;
        }
        if let Some(n) = model.navigation(&type_name, name) {
            let items: Vec<&Map<String, Json>> = match json {
                Json::Object(o) if !n.collection => vec![o],
                Json::Null if !n.collection => Vec::new(),
                Json::Array(a) if n.collection => a
                    .iter()
                    .map(|i| {
                        i.as_object().ok_or_else(|| {
                            ODataError::bad_request(format!("{name} must hold objects"))
                        })
                    })
                    .collect::<Result<_, _>>()?,
                _ => return Err(ODataError::bad_request(format!("invalid value for {name}"))),
            };
            let nested = items
                .into_iter()
                .map(|o| read_entity(model, &n.target, o, depth + 1))
                .collect::<Result<_, _>>()?;
            out.nested.push((name.clone(), nested));
            continue;
        }
        let p = model
            .property(&type_name, name)
            .ok_or_else(|| ODataError::bad_request(format!("unknown property {name}")))?;
        out.values
            .push((name.clone(), read_property(model, p, json, depth)?));
    }
    Ok(out)
}

fn concrete_type(
    model: &Model,
    declared: &str,
    obj: &Map<String, Json>,
) -> Result<String, ODataError> {
    let Some(text) = obj
        .get("@odata.type")
        .or(obj.get("@type"))
        .and_then(Json::as_str)
    else {
        return Ok(declared.to_owned());
    };
    let local = model.local(text.trim_start_matches('#'));
    let t = model
        .entity_type(local)
        .ok_or_else(|| ODataError::bad_request(format!("unknown type {text}")))?;
    if !model.type_chain(&t.name).iter().any(|b| b.name == declared) {
        return Err(ODataError::bad_request(format!(
            "{text} does not derive from {declared}"
        )));
    }
    Ok(t.name.clone())
}

pub fn read_property(
    model: &Model,
    p: &Property,
    json: &Json,
    depth: u32,
) -> Result<PropertyValue, ODataError> {
    if depth > MAX_NESTING {
        return Err(ODataError::bad_request("the payload is nested too deeply"));
    }
    if p.collection.is_some() {
        let Json::Array(items) = json else {
            return Err(ODataError::bad_request(format!(
                "{} must be an array",
                p.name
            )));
        };
        let items = items
            .iter()
            .map(|i| read_single(model, p, i, depth))
            .collect::<Result<_, _>>()?;
        return Ok(PropertyValue::Collection(items));
    }
    read_single(model, p, json, depth)
}

fn read_single(
    model: &Model,
    p: &Property,
    json: &Json,
    depth: u32,
) -> Result<PropertyValue, ODataError> {
    if json.is_null() {
        if !p.nullable {
            return Err(ODataError::bad_request(format!(
                "{} cannot be null",
                p.name
            )));
        }
        return Ok(match &p.kind {
            TypeKind::Complex(_) => PropertyValue::Complex(None),
            _ => PropertyValue::Value(Value::Null),
        });
    }
    match &p.kind {
        TypeKind::Primitive(k) => {
            let v = read_primitive(*k, json)
                .map_err(|m| ODataError::bad_request(format!("{}: {m}", p.name)))?;
            if let (Some(max), Value::String(s)) = (p.facets.max_length, &v)
                && s.chars().count() > max as usize
            {
                return Err(ODataError::bad_request(format!(
                    "{} exceeds its maximum length of {max}",
                    p.name
                )));
            }
            Ok(PropertyValue::Value(v))
        }
        TypeKind::Enum(n) => {
            let e = model
                .enum_type(n)
                .ok_or_else(|| ODataError::bad_request(format!("unknown enum {n}")))?;
            let member = match json {
                Json::String(s) => s.clone(),
                Json::Number(_) => {
                    let v = json.as_i64().unwrap_or_default();
                    e.member_name(v).map(str::to_owned).ok_or_else(|| {
                        ODataError::bad_request(format!("{v} is not a member of {n}"))
                    })?
                }
                _ => return Err(ODataError::bad_request(format!("{}: invalid enum", p.name))),
            };
            for m in member.split(',') {
                if e.member_value(m.trim()).is_none() {
                    return Err(ODataError::bad_request(format!(
                        "{m} is not a member of {n}"
                    )));
                }
            }
            Ok(PropertyValue::Value(Value::Enum {
                type_name: n.clone(),
                member,
            }))
        }
        TypeKind::Complex(n) => {
            let Json::Object(obj) = json else {
                return Err(ODataError::bad_request(format!(
                    "{} must be an object",
                    p.name
                )));
            };
            let c = model
                .complex_type(n)
                .ok_or_else(|| ODataError::bad_request(format!("unknown complex type {n}")))?;
            let mut fields = Vec::with_capacity(obj.len());
            for (name, v) in obj {
                if name.contains('@') {
                    continue;
                }
                let inner = c
                    .properties
                    .iter()
                    .find(|i| i.name == *name)
                    .ok_or_else(|| ODataError::bad_request(format!("unknown property {name}")))?;
                fields.push((name.clone(), read_property(model, inner, v, depth + 1)?));
            }
            Ok(PropertyValue::Complex(Some(fields)))
        }
    }
}

fn read_primitive(kind: PrimitiveKind, json: &Json) -> Result<Value, String> {
    use PrimitiveKind as P;
    let bad = || format!("expected {}", kind.edm_name());
    match (kind, json) {
        (P::Boolean, Json::Bool(b)) => Ok(Value::Boolean(*b)),
        (k, Json::Number(n)) if k.is_numeric() => {
            Value::parse_text(k, &n.to_string()).map_err(|_| bad())
        }
        (P::Int64 | P::Decimal, Json::String(s)) => Value::parse_text(kind, s).map_err(|_| bad()),
        (P::Single | P::Double, Json::String(s))
            if matches!(s.as_str(), "NaN" | "INF" | "-INF") =>
        {
            Value::parse_text(kind, s).map_err(|_| bad())
        }
        (P::Binary, Json::String(s)) => {
            let url_safe = s.replace('+', "-").replace('/', "_");
            Value::parse_text(kind, &url_safe).map_err(|_| bad())
        }
        (k, Json::Object(_)) if k.is_geo() => Ok(Value::Geo(json.to_string())),
        (k, Json::String(s)) if !k.is_numeric() && k != P::Boolean => {
            Value::parse_text(k, s).map_err(|_| bad())
        }
        _ => Err(bad()),
    }
}
