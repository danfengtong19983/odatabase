//! CSDL XML, the default representation of `$metadata`.

use crate::model::*;
use std::collections::BTreeSet;
use std::fmt::Write;

/// Renders `$metadata` as CSDL XML.
pub fn to_csdl_xml(m: &Model) -> String {
    let mut w = Xml {
        out: String::with_capacity(4096),
        model: m,
    };
    w.out
        .push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    w.out.push_str(
        "<edmx:Edmx Version=\"4.0\" xmlns:edmx=\"http://docs.oasis-open.org/odata/ns/edmx\">\n",
    );
    for (alias, namespace, uri) in referenced_vocabularies(m) {
        let _ = writeln!(
            w.out,
            "<edmx:Reference Uri=\"{}\"><edmx:Include Namespace=\"{namespace}\" Alias=\"{alias}\"/></edmx:Reference>",
            esc(uri)
        );
    }
    w.out.push_str("<edmx:DataServices>\n");
    let _ = writeln!(
        w.out,
        "<Schema Namespace=\"{}\" xmlns=\"http://docs.oasis-open.org/odata/ns/edm\">",
        esc(&m.namespace)
    );
    for t in m.enum_types.values() {
        w.enum_type(t);
    }
    for t in m.complex_types.values() {
        w.complex_type(t);
    }
    for t in m.entity_types.values() {
        w.entity_type(t);
    }
    for ops in m.functions.values() {
        for op in ops {
            w.operation("Function", op);
        }
    }
    for ops in m.actions.values() {
        for op in ops {
            w.operation("Action", op);
        }
    }
    w.container();
    w.out
        .push_str("</Schema>\n</edmx:DataServices>\n</edmx:Edmx>\n");
    w.out
}

/// The vocabularies used by any annotation, in stable order.
pub(crate) fn referenced_vocabularies(
    m: &Model,
) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut terms = BTreeSet::new();
    let mut add = |list: &[Annotation]| {
        for a in list {
            terms.insert(a.term.clone());
        }
    };
    add(&m.annotations);
    for t in m.entity_types.values() {
        add(&t.annotations);
        for p in &t.properties {
            add(&p.annotations);
        }
        for n in &t.navigation {
            add(&n.annotations);
        }
    }
    for t in m.complex_types.values() {
        add(&t.annotations);
        for p in &t.properties {
            add(&p.annotations);
        }
    }
    for s in m.entity_sets.values() {
        add(&s.annotations);
    }
    for s in m.singletons.values() {
        add(&s.annotations);
    }
    for ops in m.functions.values().chain(m.actions.values()) {
        for op in ops {
            add(&op.annotations);
        }
    }
    VOCABULARIES
        .into_iter()
        .filter(|(alias, namespace, _)| {
            terms.iter().any(|t| {
                t.strip_prefix(alias).is_some_and(|r| r.starts_with('.'))
                    || t.strip_prefix(namespace)
                        .is_some_and(|r| r.starts_with('.'))
            })
        })
        .collect()
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

struct Xml<'a> {
    out: String,
    model: &'a Model,
}

impl Xml<'_> {
    fn enum_type(&mut self, t: &EnumType) {
        let _ = write!(self.out, "<EnumType Name=\"{}\"", esc(&t.name));
        if t.is_flags {
            self.out.push_str(" IsFlags=\"true\"");
        }
        self.out.push('>');
        for (name, value) in &t.members {
            let _ = write!(
                self.out,
                "<Member Name=\"{}\" Value=\"{value}\"/>",
                esc(name)
            );
        }
        self.out.push_str("</EnumType>\n");
    }

    fn complex_type(&mut self, t: &ComplexType) {
        let _ = writeln!(self.out, "<ComplexType Name=\"{}\">", esc(&t.name));
        for p in &t.properties {
            self.property(p);
        }
        self.annotations(&t.annotations);
        self.out.push_str("</ComplexType>\n");
    }

    fn entity_type(&mut self, t: &EntityType) {
        let _ = write!(self.out, "<EntityType Name=\"{}\"", esc(&t.name));
        if let Some(base) = &t.base_type {
            let _ = write!(
                self.out,
                " BaseType=\"{}\"",
                esc(&self.model.qualified(base))
            );
        }
        if t.is_abstract {
            self.out.push_str(" Abstract=\"true\"");
        }
        if t.media.is_some() {
            self.out.push_str(" HasStream=\"true\"");
        }
        self.out.push_str(">\n");
        if !t.key.is_empty() {
            self.out.push_str("<Key>");
            for k in &t.key {
                let _ = write!(self.out, "<PropertyRef Name=\"{}\"/>", esc(k));
            }
            self.out.push_str("</Key>\n");
        }
        for p in &t.properties {
            self.property(p);
        }
        for n in &t.navigation {
            self.navigation(n);
        }
        self.annotations(&t.annotations);
        self.out.push_str("</EntityType>\n");
    }

    fn property(&mut self, p: &Property) {
        let name = p.type_name(self.model);
        let type_name = if p.collection.is_some() {
            format!("Collection({name})")
        } else {
            name
        };
        let _ = write!(
            self.out,
            "<Property Name=\"{}\" Type=\"{}\"",
            esc(&p.name),
            esc(&type_name)
        );
        if !p.nullable {
            self.out.push_str(" Nullable=\"false\"");
        }
        if let Some(v) = p.facets.max_length {
            let _ = write!(self.out, " MaxLength=\"{v}\"");
        }
        if let Some(v) = p.facets.precision {
            let _ = write!(self.out, " Precision=\"{v}\"");
        }
        if let Some(v) = p.facets.scale {
            let _ = write!(self.out, " Scale=\"{v}\"");
        }
        if let Some(v) = p.facets.srid {
            let _ = write!(self.out, " SRID=\"{v}\"");
        }
        self.close("Property", &p.annotations);
    }

    fn navigation(&mut self, n: &NavigationProperty) {
        let target = self.model.qualified(&n.target);
        let type_name = if n.collection {
            format!("Collection({target})")
        } else {
            target
        };
        let _ = write!(
            self.out,
            "<NavigationProperty Name=\"{}\" Type=\"{}\"",
            esc(&n.name),
            esc(&type_name)
        );
        if !n.collection && !n.nullable {
            self.out.push_str(" Nullable=\"false\"");
        }
        if let Some(partner) = &n.partner {
            let _ = write!(self.out, " Partner=\"{}\"", esc(partner));
        }
        if n.contains_target {
            self.out.push_str(" ContainsTarget=\"true\"");
        }
        self.close("NavigationProperty", &n.annotations);
    }

    fn operation(&mut self, element: &str, op: &Operation) {
        let _ = write!(self.out, "<{element} Name=\"{}\"", esc(&op.name));
        if op.is_bound {
            self.out.push_str(" IsBound=\"true\"");
        }
        if op.is_composable {
            self.out.push_str(" IsComposable=\"true\"");
        }
        self.out.push_str(">\n");
        for p in &op.parameters {
            let _ = writeln!(
                self.out,
                "<Parameter Name=\"{}\" Type=\"{}\"{}/>",
                esc(&p.name),
                esc(&wrapped_type(self.model, &p.ty)),
                nullable_attr(&p.ty)
            );
        }
        if let Some(ret) = &op.return_type {
            let _ = writeln!(
                self.out,
                "<ReturnType Type=\"{}\"{}/>",
                esc(&wrapped_type(self.model, ret)),
                nullable_attr(ret)
            );
        }
        self.annotations(&op.annotations);
        let _ = writeln!(self.out, "</{element}>");
    }

    fn container(&mut self) {
        let m = self.model;
        let _ = writeln!(self.out, "<EntityContainer Name=\"{}\">", esc(&m.container));
        for s in m.entity_sets.values() {
            let _ = write!(
                self.out,
                "<EntitySet Name=\"{}\" EntityType=\"{}\"",
                esc(&s.name),
                esc(&m.qualified(&s.entity_type))
            );
            if !s.include_in_service_document {
                self.out.push_str(" IncludeInServiceDocument=\"false\"");
            }
            self.out.push('>');
            self.bindings(&s.entity_type);
            self.annotations(&s.annotations);
            self.out.push_str("</EntitySet>\n");
        }
        for s in m.singletons.values() {
            let _ = write!(
                self.out,
                "<Singleton Name=\"{}\" Type=\"{}\">",
                esc(&s.name),
                esc(&m.qualified(&s.entity_type))
            );
            self.bindings(&s.entity_type);
            self.annotations(&s.annotations);
            self.out.push_str("</Singleton>\n");
        }
        for (name, ops) in &m.functions {
            if ops.iter().any(|o| !o.is_bound) {
                let target = entity_set_of(m, ops);
                let listed = if in_service_document(ops) {
                    " IncludeInServiceDocument=\"true\""
                } else {
                    ""
                };
                let _ = writeln!(
                    self.out,
                    "<FunctionImport Name=\"{0}\" Function=\"{1}\"{target}{listed}/>",
                    esc(name),
                    esc(&m.qualified(name))
                );
            }
        }
        for (name, ops) in &m.actions {
            if ops.iter().any(|o| !o.is_bound) {
                let target = entity_set_of(m, ops);
                let _ = writeln!(
                    self.out,
                    "<ActionImport Name=\"{0}\" Action=\"{1}\"{target}/>",
                    esc(name),
                    esc(&m.qualified(name))
                );
            }
        }
        self.annotations(&m.annotations);
        self.out.push_str("</EntityContainer>\n");
    }

    fn bindings(&mut self, type_name: &str) {
        let m = self.model;
        for n in m.all_navigation(type_name) {
            if let Some(set) = m.entity_set_of_type(&n.target) {
                let _ = write!(
                    self.out,
                    "<NavigationPropertyBinding Path=\"{}\" Target=\"{}\"/>",
                    esc(&n.name),
                    esc(&set.name)
                );
            }
        }
    }

    fn close(&mut self, element: &str, annotations: &[Annotation]) {
        if annotations.is_empty() {
            self.out.push_str("/>\n");
        } else {
            self.out.push('>');
            self.annotations(annotations);
            let _ = writeln!(self.out, "</{element}>");
        }
    }

    fn annotations(&mut self, list: &[Annotation]) {
        for a in list {
            let _ = write!(self.out, "<Annotation Term=\"{}\"", esc(&a.term));
            match &a.value {
                AnnotationValue::Bool(v) => {
                    let _ = write!(self.out, " Bool=\"{v}\"/>");
                }
                AnnotationValue::Int(v) => {
                    let _ = write!(self.out, " Int=\"{v}\"/>");
                }
                AnnotationValue::String(v) => {
                    let _ = write!(self.out, " String=\"{}\"/>", esc(v));
                }
                AnnotationValue::EnumMember(v) => {
                    let _ = write!(self.out, " EnumMember=\"{}\"/>", esc(v));
                }
                AnnotationValue::Path(v) => {
                    let _ = write!(self.out, " Path=\"{}\"/>", esc(v));
                }
                other => {
                    self.out.push('>');
                    self.nested(other);
                    self.out.push_str("</Annotation>");
                }
            }
            self.out.push('\n');
        }
    }

    fn nested(&mut self, v: &AnnotationValue) {
        match v {
            AnnotationValue::Bool(v) => {
                let _ = write!(self.out, "<Bool>{v}</Bool>");
            }
            AnnotationValue::Int(v) => {
                let _ = write!(self.out, "<Int>{v}</Int>");
            }
            AnnotationValue::String(v) => {
                let _ = write!(self.out, "<String>{}</String>", esc(v));
            }
            AnnotationValue::EnumMember(v) => {
                let _ = write!(self.out, "<EnumMember>{}</EnumMember>", esc(v));
            }
            AnnotationValue::Path(v) => {
                let _ = write!(self.out, "<Path>{}</Path>", esc(v));
            }
            AnnotationValue::Collection(items) => {
                self.out.push_str("<Collection>");
                for item in items {
                    self.nested(item);
                }
                self.out.push_str("</Collection>");
            }
            AnnotationValue::Record(fields) => {
                self.out.push_str("<Record>");
                for (name, value) in fields {
                    let _ = write!(self.out, "<PropertyValue Property=\"{}\">", esc(name));
                    self.nested(value);
                    self.out.push_str("</PropertyValue>");
                }
                self.out.push_str("</Record>");
            }
        }
    }
}

/// A function can only be listed when it can be called without parameters.
pub(crate) fn in_service_document(ops: &[Operation]) -> bool {
    ops.iter()
        .filter(|o| !o.is_bound)
        .all(|o| o.parameters.is_empty())
}

fn wrapped_type(m: &Model, t: &OperationType) -> String {
    let name = t.type_name(m);
    if t.collection {
        format!("Collection({name})")
    } else {
        name
    }
}

fn nullable_attr(t: &OperationType) -> &'static str {
    if t.nullable {
        ""
    } else {
        " Nullable=\"false\""
    }
}

fn entity_set_of(m: &Model, ops: &[Operation]) -> String {
    ops.iter()
        .filter_map(|o| match o.return_type.as_ref().map(|r| &r.kind) {
            Some(OperationTypeKind::Entity(n)) => m.entity_set_of_type(n),
            _ => None,
        })
        .map(|s| format!(" EntitySet=\"{}\"", esc(&s.name)))
        .next()
        .unwrap_or_default()
}
