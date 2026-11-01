//! OData JSON format of entities, values and collections.

use crate::format::{Format, MetadataLevel};
use crate::url::{encode, entity_id};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use odatabase_edm::{Model, PrimitiveKind, Value};
use odatabase_sql::{Node, Record};
use serde_json::{Map, Number, Value as Json};

/// Inputs shared by everything in one response.
#[derive(Clone, Copy)]
pub struct Writer<'a> {
    pub model: &'a Model,
    pub root: &'a str,
    pub format: Format,
}

impl Writer<'_> {
    pub fn value(&self, v: &Value) -> Json {
        match v {
            Value::Null => Json::Null,
            Value::Boolean(b) => Json::Bool(*b),
            Value::Byte(_) | Value::SByte(_) | Value::Int16(_) | Value::Int32(_) => {
                Json::Number(v.as_i64().unwrap_or_default().into())
            }
            Value::Int64(n) if self.format.ieee754 => Json::String(n.to_string()),
            Value::Int64(n) => Json::Number((*n).into()),
            Value::Single(f) => float(f64::from(*f), v),
            Value::Double(f) => float(*f, v),
            Value::Decimal(_) if self.format.ieee754 => Json::String(v.to_text()),
            Value::Decimal(_) => Number::from_string_unchecked(v.to_text()).into(),
            Value::Binary(b) => Json::String(URL_SAFE_NO_PAD.encode(b)),
            Value::Geo(text) => {
                serde_json::from_str(text).unwrap_or_else(|_| Json::String(text.clone()))
            }
            other => Json::String(other.to_text()),
        }
    }

    pub fn node(&self, n: &Node) -> Json {
        match n {
            Node::Value(v) => self.value(v),
            Node::Complex(None) | Node::Entity(None) => Json::Null,
            Node::Complex(Some(r)) => self.complex(r).into(),
            Node::Collection(items) => items.iter().map(|i| self.node(i)).collect(),
            Node::Entity(Some(r)) => self.entity(r, &r.type_name).into(),
            Node::Entities { items, .. } => items
                .iter()
                .map(|r| Json::Object(self.entity(r, &r.type_name)))
                .collect(),
            Node::Count(c) => Json::Number((*c).into()),
            Node::Refs(_) => Json::Null,
        }
    }

    fn complex(&self, r: &Record) -> Map<String, Json> {
        r.fields
            .iter()
            .map(|(k, v)| (k.clone(), self.node(v)))
            .collect()
    }

    /// Serializes `rec`. `declared` is the type the context URL promises.
    pub fn entity(&self, rec: &Record, declared: &str) -> Map<String, Json> {
        let full = self.format.metadata == MetadataLevel::Full;
        let none = self.format.metadata == MetadataLevel::None;
        let mut out = Map::new();
        let concrete = &rec.type_name;
        if !none && (full || concrete != declared) {
            out.insert(
                "@odata.type".into(),
                format!("#{}", self.model.qualified(concrete)).into(),
            );
        }
        let id = entity_id(self.model, concrete, rec);
        if full && let Some(id) = &id {
            out.insert("@odata.id".into(), format!("{}/{id}", self.root).into());
            out.insert("@odata.editLink".into(), id.clone().into());
        }
        if !none && let Some(etag) = self.etag(rec) {
            out.insert("@odata.etag".into(), etag.into());
        }
        if full
            && let Some(id) = &id
            && self
                .model
                .type_chain(concrete)
                .iter()
                .any(|t| t.media.is_some())
        {
            out.insert("@odata.mediaReadLink".into(), format!("{id}/$value").into());
        }
        for (name, node) in &rec.fields {
            match node {
                Node::Count(c) => {
                    if !none {
                        out.insert(format!("{name}@odata.count"), (*c).into());
                    }
                }
                Node::Refs(refs) => {
                    let link = |r: &Record| {
                        let mut o = Map::new();
                        if let Some(id) = entity_id(self.model, &r.type_name, r) {
                            o.insert("@odata.id".into(), format!("{}/{id}", self.root).into());
                        }
                        Json::Object(o)
                    };
                    let v = if refs.collection {
                        refs.items.iter().map(link).collect()
                    } else {
                        refs.items.first().map_or(Json::Null, link)
                    };
                    out.insert(name.clone(), v);
                }
                Node::Entities {
                    count, next_link, ..
                } => {
                    if let Some(c) = count
                        && !none
                    {
                        out.insert(format!("{name}@odata.count"), (*c).into());
                    }
                    if let Some(next) = next_link {
                        out.insert(format!("{name}@odata.nextLink"), next.clone().into());
                    }
                    out.insert(name.clone(), self.related(concrete, name, node));
                }
                Node::Entity(_) => {
                    out.insert(name.clone(), self.related(concrete, name, node));
                }
                Node::Value(v) => {
                    if full && let Some(t) = odata_type(v) {
                        out.insert(format!("{name}@odata.type"), t.into());
                    }
                    out.insert(name.clone(), self.value(v));
                }
                other => {
                    out.insert(name.clone(), self.node(other));
                }
            }
        }
        if full && let Some(id) = &id {
            for n in self.model.all_navigation(concrete) {
                out.insert(
                    format!("{}@odata.navigationLink", n.name),
                    format!("{id}/{}", encode(&n.name)).into(),
                );
            }
        }
        out
    }

    /// Serializes an expanded navigation against the type the navigation declares.
    fn related(&self, owner: &str, name: &str, node: &Node) -> Json {
        let declared = self
            .model
            .navigation(owner, name)
            .map(|n| n.target.as_str());
        let write = |r: &Record| Json::Object(self.entity(r, declared.unwrap_or(&r.type_name)));
        match node {
            Node::Entity(Some(r)) => write(r),
            Node::Entities { items, .. } => items.iter().map(write).collect(),
            _ => Json::Null,
        }
    }

    pub fn etag(&self, rec: &Record) -> Option<String> {
        let p = self.model.etag_property(&rec.type_name)?;
        rec.value(&p.name).map(etag_header)
    }
}

/// The weak entity tag of a version value.
pub fn etag_header(v: &Value) -> String {
    format!("W/\"{}\"", encode(&v.to_text()))
}

fn float(f: f64, v: &Value) -> Json {
    match Number::from_f64(f) {
        Some(n) if f.is_finite() => match v {
            // f32 widens with noise: its shortest text form is what was stored.
            Value::Single(_) => Number::from_string_unchecked(v.to_text()).into(),
            _ => n.into(),
        },
        _ => Json::String(v.to_text()),
    }
}

/// `@odata.type` of a value whose JSON form does not imply its Edm type.
fn odata_type(v: &Value) -> Option<String> {
    let kind = v.kind()?;
    match kind {
        PrimitiveKind::Boolean | PrimitiveKind::String | PrimitiveKind::Double => None,
        k => Some(format!("#{}", k.edm_name().trim_start_matches("Edm."))),
    }
}
