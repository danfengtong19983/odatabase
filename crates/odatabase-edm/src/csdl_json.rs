//! CSDL JSON, served for `$metadata?$format=json`.

use crate::csdl_xml::{in_service_document, referenced_vocabularies};
use crate::model::*;
use crate::value::PrimitiveKind;
use serde_json::{Map, Value as Json, json};

pub fn to_csdl_json(m: &Model) -> Json {
    let mut schema = Map::new();
    for t in m.enum_types.values() {
        schema.insert(t.name.clone(), enum_type(t));
    }
    for t in m.complex_types.values() {
        let mut o = Map::new();
        o.insert("$Kind".into(), "ComplexType".into());
        for p in &t.properties {
            o.insert(p.name.clone(), property(m, p));
        }
        annotations(&mut o, &t.annotations);
        schema.insert(t.name.clone(), o.into());
    }
    for t in m.entity_types.values() {
        schema.insert(t.name.clone(), entity_type(m, t));
    }
    for (name, ops) in &m.functions {
        schema.insert(
            name.clone(),
            ops.iter().map(|o| operation(m, "Function", o)).collect(),
        );
    }
    for (name, ops) in &m.actions {
        schema.insert(
            name.clone(),
            ops.iter().map(|o| operation(m, "Action", o)).collect(),
        );
    }
    schema.insert(m.container.clone(), container(m));

    let mut root = Map::new();
    root.insert("$Version".into(), "4.0".into());
    root.insert("$EntityContainer".into(), m.qualified(&m.container).into());
    let refs: Map<String, Json> = referenced_vocabularies(m)
        .into_iter()
        .map(|(alias, namespace, uri)| {
            (
                uri.to_owned(),
                json!({"$Include": [{"$Namespace": namespace, "$Alias": alias}]}),
            )
        })
        .collect();
    if !refs.is_empty() {
        root.insert("$Reference".into(), refs.into());
    }
    root.insert(m.namespace.clone(), schema.into());
    root.into()
}

fn enum_type(t: &EnumType) -> Json {
    let mut o = Map::new();
    o.insert("$Kind".into(), "EnumType".into());
    if t.is_flags {
        o.insert("$IsFlags".into(), true.into());
    }
    for (name, value) in &t.members {
        o.insert(name.clone(), (*value).into());
    }
    o.into()
}

fn entity_type(m: &Model, t: &EntityType) -> Json {
    let mut o = Map::new();
    o.insert("$Kind".into(), "EntityType".into());
    if let Some(base) = &t.base_type {
        o.insert("$BaseType".into(), m.qualified(base).into());
    }
    if t.is_abstract {
        o.insert("$Abstract".into(), true.into());
    }
    if t.media.is_some() {
        o.insert("$HasStream".into(), true.into());
    }
    if !t.key.is_empty() {
        o.insert("$Key".into(), t.key.clone().into());
    }
    for p in &t.properties {
        o.insert(p.name.clone(), property(m, p));
    }
    for n in &t.navigation {
        let mut nav = Map::new();
        nav.insert("$Kind".into(), "NavigationProperty".into());
        nav.insert("$Type".into(), m.qualified(&n.target).into());
        if n.collection {
            nav.insert("$Collection".into(), true.into());
        } else if n.nullable {
            nav.insert("$Nullable".into(), true.into());
        }
        if let Some(partner) = &n.partner {
            nav.insert("$Partner".into(), partner.clone().into());
        }
        if n.contains_target {
            nav.insert("$ContainsTarget".into(), true.into());
        }
        annotations(&mut nav, &n.annotations);
        o.insert(n.name.clone(), nav.into());
    }
    annotations(&mut o, &t.annotations);
    o.into()
}

fn property(m: &Model, p: &Property) -> Json {
    let mut o = Map::new();
    if p.kind != TypeKind::Primitive(PrimitiveKind::String) {
        o.insert("$Type".into(), p.type_name(m).into());
    }
    if p.collection.is_some() {
        o.insert("$Collection".into(), true.into());
    }
    if p.nullable {
        o.insert("$Nullable".into(), true.into());
    }
    if let Some(v) = p.facets.max_length {
        o.insert("$MaxLength".into(), v.into());
    }
    if let Some(v) = p.facets.precision {
        o.insert("$Precision".into(), v.into());
    }
    if let Some(v) = p.facets.scale {
        o.insert("$Scale".into(), v.into());
    }
    if let Some(v) = p.facets.srid {
        o.insert("$SRID".into(), v.into());
    }
    annotations(&mut o, &p.annotations);
    o.into()
}

fn operation(m: &Model, kind: &str, op: &Operation) -> Json {
    let mut o = Map::new();
    o.insert("$Kind".into(), kind.into());
    if op.is_bound {
        o.insert("$IsBound".into(), true.into());
    }
    if op.is_composable {
        o.insert("$IsComposable".into(), true.into());
    }
    if !op.parameters.is_empty() {
        let params: Vec<Json> = op
            .parameters
            .iter()
            .map(|p| {
                let mut v = type_ref(m, &p.ty);
                v.insert("$Name".into(), p.name.clone().into());
                v.into()
            })
            .collect();
        o.insert("$Parameter".into(), params.into());
    }
    if let Some(ret) = &op.return_type {
        o.insert("$ReturnType".into(), type_ref(m, ret).into());
    }
    annotations(&mut o, &op.annotations);
    o.into()
}

fn type_ref(m: &Model, t: &OperationType) -> Map<String, Json> {
    let mut o = Map::new();
    o.insert("$Type".into(), t.type_name(m).into());
    if t.collection {
        o.insert("$Collection".into(), true.into());
    }
    if t.nullable {
        o.insert("$Nullable".into(), true.into());
    }
    o
}

fn container(m: &Model) -> Json {
    let mut o = Map::new();
    o.insert("$Kind".into(), "EntityContainer".into());
    for s in m.entity_sets.values() {
        let mut e = Map::new();
        e.insert("$Collection".into(), true.into());
        e.insert("$Type".into(), m.qualified(&s.entity_type).into());
        if !s.include_in_service_document {
            e.insert("$IncludeInServiceDocument".into(), false.into());
        }
        bindings(m, &mut e, &s.entity_type);
        annotations(&mut e, &s.annotations);
        o.insert(s.name.clone(), e.into());
    }
    for s in m.singletons.values() {
        let mut e = Map::new();
        e.insert("$Type".into(), m.qualified(&s.entity_type).into());
        bindings(m, &mut e, &s.entity_type);
        annotations(&mut e, &s.annotations);
        o.insert(s.name.clone(), e.into());
    }
    for (name, ops) in &m.functions {
        if ops.iter().any(|f| !f.is_bound) {
            o.insert(
                name.clone(),
                if in_service_document(ops) {
                    json!({"$Function": m.qualified(name), "$IncludeInServiceDocument": true})
                } else {
                    json!({"$Function": m.qualified(name)})
                },
            );
        }
    }
    for (name, ops) in &m.actions {
        if ops.iter().any(|a| !a.is_bound) {
            o.insert(name.clone(), json!({"$Action": m.qualified(name)}));
        }
    }
    annotations(&mut o, &m.annotations);
    o.into()
}

fn bindings(m: &Model, o: &mut Map<String, Json>, type_name: &str) {
    let map: Map<String, Json> = m
        .all_navigation(type_name)
        .into_iter()
        .filter_map(|n| {
            m.entity_set_of_type(&n.target)
                .map(|s| (n.name.clone(), s.name.clone().into()))
        })
        .collect();
    if !map.is_empty() {
        o.insert("$NavigationPropertyBinding".into(), map.into());
    }
}

fn annotations(o: &mut Map<String, Json>, list: &[Annotation]) {
    for a in list {
        o.insert(format!("@{}", a.term), annotation_value(&a.value));
    }
}

fn annotation_value(v: &AnnotationValue) -> Json {
    match v {
        AnnotationValue::Bool(v) => (*v).into(),
        AnnotationValue::Int(v) => (*v).into(),
        AnnotationValue::String(v) => v.clone().into(),
        AnnotationValue::EnumMember(v) => json!({"$EnumMember": v}),
        AnnotationValue::Path(v) => json!({"$Path": v}),
        AnnotationValue::Collection(items) => items.iter().map(annotation_value).collect(),
        AnnotationValue::Record(fields) => {
            let map: Map<String, Json> = fields
                .iter()
                .map(|(k, v)| (k.clone(), annotation_value(v)))
                .collect();
            map.into()
        }
    }
}
