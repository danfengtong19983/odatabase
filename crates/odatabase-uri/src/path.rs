//! Resource paths and the top-level query string.

use crate::UriError;
use crate::ast::*;
use crate::expr::{Aliases, Scope, parse_expression, property_type};
use crate::options::{OptionContext, apply_option, split_top_level};
use odatabase_edm::{Model, Operation, OperationType, OperationTypeKind, TypeKind, Value};
use percent_encoding::percent_decode_str;

/// Bounds on request complexity. Exceeding one is a 400.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_expression_depth: u32,
    pub max_expand_depth: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_expression_depth: 64,
            max_expand_depth: 8,
        }
    }
}

/// Parses a URL relative to the service root. `path` excludes the root, `query` the `?`.
pub fn parse(model: &Model, path: &str, query: &str) -> Result<ODataUri, UriError> {
    parse_with(model, path, query, Limits::default())
}

pub fn parse_with(
    model: &Model,
    path: &str,
    query: &str,
    limits: Limits,
) -> Result<ODataUri, UriError> {
    let raw = decode_query(query)?;
    let mut aliases = Aliases::new();
    for (name, value) in &raw {
        if let Some(alias) = name.strip_prefix('@') {
            let max = limits.max_expression_depth;
            let e = parse_expression(model, value, Scope::None, &Aliases::new(), max)?;
            aliases.insert(alias.to_owned(), e);
        }
    }
    let segments = parse_path(model, path, &aliases, limits)?;
    let mut options = QueryOptions::default();
    let scope = match segments.last().map(Segment::target) {
        Some(Target::EntityCollection(t) | Target::Entity(t)) => Scope::Entity(t.clone()),
        Some(Target::Complex(t) | Target::ComplexCollection(t)) => Scope::Complex(t.clone()),
        Some(Target::Ref { .. }) => match segments.iter().rev().nth(1).map(Segment::target) {
            Some(Target::EntityCollection(t) | Target::Entity(t)) => Scope::Entity(t.clone()),
            _ => Scope::None,
        },
        Some(Target::Count) => match segments.iter().rev().nth(1).map(Segment::target) {
            Some(Target::EntityCollection(t)) => Scope::Entity(t.clone()),
            _ => Scope::None,
        },
        _ => Scope::None,
    };
    let ctx = OptionContext {
        model,
        aliases: &aliases,
        limits,
        expand_depth: 0,
    };
    for (name, value) in &raw {
        if name.starts_with('$') {
            if raw.iter().filter(|(n, _)| n == name).count() > 1 {
                return Err(UriError::bad_request(format!("{name} is given twice")));
            }
            apply_option(&ctx, &scope, &mut options, name, value)?;
        } else if !name.starts_with('@') {
            options.custom.push((name.clone(), value.clone()));
        }
    }
    Ok(ODataUri {
        path: segments,
        options,
    })
}

fn decode_query(query: &str) -> Result<Vec<(String, String)>, UriError> {
    let mut out = Vec::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        out.push((decode(name)?, decode(value)?));
    }
    Ok(out)
}

fn decode(text: &str) -> Result<String, UriError> {
    percent_decode_str(text)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| UriError::bad_request("URL is not valid UTF-8"))
}

fn split_path(path: &str) -> Vec<&str> {
    split_top_level(path.trim_matches('/'), '/')
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_path(
    model: &Model,
    path: &str,
    aliases: &Aliases,
    limits: Limits,
) -> Result<Vec<Segment>, UriError> {
    let mut b = PathBuilder {
        model,
        aliases,
        limits,
        out: Vec::new(),
        current: Target::Root,
    };
    let parts = split_path(path);
    for (i, part) in parts.iter().enumerate() {
        let part = decode(part)?;
        b.segment(&part, i + 1 == parts.len())?;
    }
    Ok(b.out)
}

struct PathBuilder<'a> {
    model: &'a Model,
    aliases: &'a Aliases,
    limits: Limits,
    out: Vec<Segment>,
    current: Target,
}

/// `Name(args)` to its parts. A bare name has no parentheses.
fn split_call(text: &str) -> Result<(&str, Option<&str>), UriError> {
    match text.find('(') {
        None => Ok((text, None)),
        Some(i) => match text.strip_suffix(')') {
            Some(t) => Ok((&t[..i], Some(&t[i + 1..]))),
            None => Err(UriError::bad_request(format!(
                "unbalanced parentheses in {text}"
            ))),
        },
    }
}

impl PathBuilder<'_> {
    fn push(&mut self, s: Segment) {
        self.current = s.target().clone();
        self.out.push(s);
    }

    fn segment(&mut self, text: &str, last: bool) -> Result<(), UriError> {
        if self.current == Target::Root {
            return self.first(text, last);
        }
        match (text, &self.current) {
            ("$count", Target::EntityCollection(_))
            | ("$count", Target::ComplexCollection(_))
            | ("$count", Target::PrimitiveCollection(_)) => {
                self.require_last(text, last)?;
                self.push(Segment::Count);
                return Ok(());
            }
            ("$count", _) => {
                return Err(UriError::bad_request("$count needs a collection"));
            }
            ("$value", Target::Primitive(_)) => {
                self.require_last(text, last)?;
                self.push(Segment::Value);
                return Ok(());
            }
            ("$value", Target::Entity(t)) => {
                if self.model.type_chain(t).iter().all(|e| e.media.is_none()) {
                    return Err(UriError::bad_request(format!("{t} has no stream")));
                }
                self.require_last(text, last)?;
                self.push(Segment::Value);
                return Ok(());
            }
            ("$value", _) => {
                return Err(UriError::bad_request("$value needs a primitive value"));
            }
            ("$ref", Target::EntityCollection(_) | Target::Entity(_)) => {
                self.require_last(text, last)?;
                if !self
                    .out
                    .iter()
                    .any(|s| matches!(s, Segment::Navigation { .. }))
                    && !matches!(self.out.last(), Some(Segment::Key { .. }))
                {
                    return Err(UriError::bad_request("$ref needs an entity reference"));
                }
                self.push(Segment::Ref);
                return Ok(());
            }
            _ => {}
        }
        let (name, args) = split_call(text)?;
        match self.current.clone() {
            Target::EntityCollection(t) => {
                if name.is_empty() {
                    return self.key(&t, args.unwrap_or(""));
                }
                if name.contains('.') {
                    return self.qualified(&t, true, name, args);
                }
                Err(UriError::bad_request(format!(
                    "{name}: a collection needs a key before further segments"
                )))
            }
            Target::Entity(t) => {
                if name.contains('.') {
                    return self.qualified(&t, false, name, args);
                }
                self.member(&t, name, args)
            }
            Target::Complex(t) => {
                let c = self.model.complex_type(&t).ok_or_else(|| not_found(name))?;
                let p = c
                    .properties
                    .iter()
                    .find(|p| p.name == name)
                    .ok_or_else(|| not_found(name))?;
                self.push(Segment::Property {
                    name: name.to_owned(),
                    target: property_target(p),
                });
                Ok(())
            }
            _ => Err(not_found(text)),
        }
    }

    fn require_last(&self, text: &str, last: bool) -> Result<(), UriError> {
        if last {
            Ok(())
        } else {
            Err(UriError::bad_request(format!(
                "{text} must be the last segment"
            )))
        }
    }

    fn first(&mut self, text: &str, last: bool) -> Result<(), UriError> {
        match text {
            "$metadata" => {
                self.require_last(text, last)?;
                self.push(Segment::Metadata);
                return Ok(());
            }
            "$batch" => {
                self.require_last(text, last)?;
                self.push(Segment::Batch);
                return Ok(());
            }
            "$all" | "$crossjoin" | "$entity" => {
                return Err(UriError::not_implemented(format!(
                    "{text} is not supported"
                )));
            }
            _ => {}
        }
        let (name, args) = split_call(text)?;
        if let Some(set) = self.model.entity_sets.get(name) {
            self.push(Segment::EntitySet {
                name: name.to_owned(),
                target: Target::EntityCollection(set.entity_type.clone()),
            });
            if let Some(args) = args {
                let t = set.entity_type.clone();
                self.key(&t, args)?;
            }
            return Ok(());
        }
        if let Some(s) = self.model.singletons.get(name) {
            if args.is_some() {
                return Err(UriError::bad_request("a singleton has no key"));
            }
            self.push(Segment::Singleton {
                name: name.to_owned(),
                target: Target::Entity(s.entity_type.clone()),
            });
            return Ok(());
        }
        if let Some(a) = args
            && self.model.functions.contains_key(name)
        {
            return self.function(None, name, a);
        }
        if args.is_none() && self.model.actions.contains_key(name) {
            return self.action(None, name);
        }
        Err(not_found(text))
    }

    fn key(&mut self, type_name: &str, args: &str) -> Result<(), UriError> {
        let key = self.model.key_of(type_name).to_vec();
        let parts = split_top_level(args, ',');
        let mut values = Vec::with_capacity(key.len());
        if parts.len() == 1 && key.len() == 1 && !looks_named(parts[0]) {
            values.push((
                key[0].clone(),
                self.key_value(type_name, &key[0], parts[0])?,
            ));
        } else {
            for part in parts {
                let (name, text) = part
                    .split_once('=')
                    .ok_or_else(|| UriError::bad_request("composite keys must be named"))?;
                let name = name.trim();
                if !key.iter().any(|k| k == name) {
                    return Err(UriError::bad_request(format!(
                        "{name} is not a key property"
                    )));
                }
                if values.iter().any(|(n, _)| n == name) {
                    return Err(UriError::bad_request(format!("{name} is given twice")));
                }
                values.push((name.to_owned(), self.key_value(type_name, name, text)?));
            }
        }
        if values.len() != key.len() {
            return Err(UriError::bad_request(format!(
                "{type_name} needs {} key values",
                key.len()
            )));
        }
        self.push(Segment::Key {
            values,
            target: Target::Entity(type_name.to_owned()),
        });
        Ok(())
    }

    fn key_value(&self, type_name: &str, prop: &str, text: &str) -> Result<Value, UriError> {
        let expr = self.literal(text)?;
        let Expr::Literal(v) = expr else {
            return Err(UriError::bad_request("a key value must be a literal"));
        };
        if v.is_null() {
            return Err(UriError::bad_request("a key value cannot be null"));
        }
        let p = self
            .model
            .property(type_name, prop)
            .ok_or_else(|| UriError::bad_request(format!("unknown key {prop}")))?;
        match &p.kind {
            TypeKind::Primitive(k) => v.coerce(*k).ok_or_else(|| {
                UriError::bad_request(format!("{text} is not a valid {}", k.edm_name()))
            }),
            TypeKind::Enum(_) => Ok(v),
            TypeKind::Complex(_) => {
                Err(UriError::bad_request("a complex property cannot be a key"))
            }
        }
    }

    fn literal(&self, text: &str) -> Result<Expr, UriError> {
        let depth = self.limits.max_expression_depth;
        parse_expression(self.model, text.trim(), Scope::None, self.aliases, depth)
    }

    fn member(&mut self, t: &str, name: &str, args: Option<&str>) -> Result<(), UriError> {
        if let Some(p) = self.model.property(t, name) {
            if args.is_some() && p.collection.is_none() {
                return Err(UriError::bad_request(format!("{name} has no key")));
            }
            self.push(Segment::Property {
                name: name.to_owned(),
                target: property_target(p),
            });
            return Ok(());
        }
        if let Some(n) = self.model.navigation(t, name) {
            let target = if n.collection {
                Target::EntityCollection(n.target.clone())
            } else {
                Target::Entity(n.target.clone())
            };
            self.push(Segment::Navigation {
                name: name.to_owned(),
                target,
            });
            if let Some(a) = args {
                if !n.collection {
                    return Err(UriError::bad_request(format!("{name} is single-valued")));
                }
                let target = n.target.clone();
                self.key(&target, a)?;
            }
            return Ok(());
        }
        Err(not_found(name))
    }

    /// A dotted segment is a type cast or a bound operation.
    fn qualified(
        &mut self,
        t: &str,
        collection: bool,
        name: &str,
        args: Option<&str>,
    ) -> Result<(), UriError> {
        let local = self.model.local(name);
        if let Some(a) = args
            && self.model.functions.contains_key(local)
        {
            return self.function(Some((t.to_owned(), collection)), local, a);
        }
        if args.is_none() && self.model.actions.contains_key(local) {
            return self.action(Some((t.to_owned(), collection)), local);
        }
        if self.model.entity_types.contains_key(local) {
            if !self.model.type_chain(local).iter().any(|b| b.name == t) {
                return Err(UriError::bad_request(format!(
                    "{name} does not derive from {t}"
                )));
            }
            let target = if collection {
                Target::EntityCollection(local.to_owned())
            } else {
                Target::Entity(local.to_owned())
            };
            self.push(Segment::Cast {
                type_name: local.to_owned(),
                target,
            });
            if let Some(a) = args {
                if !collection {
                    return Err(UriError::bad_request("a key needs a collection"));
                }
                self.key(local, a)?;
            }
            return Ok(());
        }
        Err(not_found(name))
    }

    fn function(
        &mut self,
        binding: Option<(String, bool)>,
        name: &str,
        args: &str,
    ) -> Result<(), UriError> {
        let mut parsed = Vec::new();
        if !args.trim().is_empty() {
            for part in split_top_level(args, ',') {
                let (n, text) = part
                    .split_once('=')
                    .ok_or_else(|| UriError::bad_request("function parameters must be named"))?;
                parsed.push((n.trim().to_owned(), self.literal(text)?));
            }
        }
        let overloads = &self.model.functions[name];
        let found = overloads.iter().enumerate().find(|(_, o)| {
            binding_matches(self.model, o, binding.as_ref()) && {
                let params = &o.parameters[usize::from(o.is_bound)..];
                params.len() == parsed.len()
                    && params
                        .iter()
                        .all(|p| parsed.iter().any(|(n, _)| *n == p.name))
            }
        });
        let Some((overload, op)) = found else {
            return Err(UriError::not_found(format!(
                "no overload of {name} matches"
            )));
        };
        for (n, value) in &mut parsed {
            let p = op
                .parameters
                .iter()
                .find(|p| p.name == *n)
                .expect("checked above");
            coerce_argument(value, &p.ty)?;
        }
        let target = operation_target(op);
        self.push(Segment::Function {
            name: name.to_owned(),
            overload,
            args: parsed,
            target,
        });
        Ok(())
    }

    fn action(&mut self, binding: Option<(String, bool)>, name: &str) -> Result<(), UriError> {
        let overloads = &self.model.actions[name];
        let found = overloads
            .iter()
            .enumerate()
            .find(|(_, o)| binding_matches(self.model, o, binding.as_ref()));
        let Some((overload, _)) = found else {
            return Err(UriError::not_found(format!(
                "no overload of {name} matches"
            )));
        };
        self.push(Segment::Action {
            name: name.to_owned(),
            overload,
            target: Target::ActionResult,
        });
        Ok(())
    }
}

fn binding_matches(model: &Model, op: &Operation, binding: Option<&(String, bool)>) -> bool {
    match (op.is_bound, binding) {
        (false, None) => true,
        (true, Some((type_name, collection))) => {
            let Some(p) = op.parameters.first() else {
                return false;
            };
            p.ty.collection == *collection
                && match &p.ty.kind {
                    OperationTypeKind::Entity(n) => {
                        model.type_chain(type_name).iter().any(|t| t.name == *n)
                    }
                    OperationTypeKind::Value(_) => false,
                }
        }
        _ => false,
    }
}

fn coerce_argument(arg: &mut Expr, ty: &OperationType) -> Result<(), UriError> {
    if let (Expr::Literal(v), OperationTypeKind::Value(TypeKind::Primitive(k))) = (&*arg, &ty.kind)
    {
        let coerced = v
            .coerce(*k)
            .ok_or_else(|| UriError::bad_request(format!("expected {}", k.edm_name())))?;
        *arg = Expr::Literal(coerced);
    }
    Ok(())
}

fn operation_target(op: &Operation) -> Target {
    let Some(ret) = &op.return_type else {
        return Target::Void;
    };
    match (&ret.kind, ret.collection) {
        (OperationTypeKind::Entity(n), false) => Target::Entity(n.clone()),
        (OperationTypeKind::Entity(n), true) => Target::EntityCollection(n.clone()),
        (OperationTypeKind::Value(TypeKind::Complex(n)), false) => Target::Complex(n.clone()),
        (OperationTypeKind::Value(TypeKind::Complex(n)), true) => {
            Target::ComplexCollection(n.clone())
        }
        (OperationTypeKind::Value(TypeKind::Primitive(k)), false) => {
            Target::Primitive(ExprType::Primitive(*k))
        }
        (OperationTypeKind::Value(TypeKind::Primitive(k)), true) => {
            Target::PrimitiveCollection(ExprType::Primitive(*k))
        }
        (OperationTypeKind::Value(TypeKind::Enum(n)), false) => {
            Target::Primitive(ExprType::Enum(n.clone()))
        }
        (OperationTypeKind::Value(TypeKind::Enum(n)), true) => {
            Target::PrimitiveCollection(ExprType::Enum(n.clone()))
        }
    }
}

fn looks_named(text: &str) -> bool {
    match text.split_once('=') {
        Some((name, _)) => !name.contains('\'') && !name.trim().is_empty(),
        None => false,
    }
}

fn not_found(name: &str) -> UriError {
    UriError::not_found(format!("resource {name} does not exist"))
}

fn property_target(p: &odatabase_edm::Property) -> Target {
    match property_type(p) {
        ExprType::Complex(n) => Target::Complex(n),
        ExprType::Collection(inner) => match *inner {
            ExprType::Complex(n) => Target::ComplexCollection(n),
            other => Target::PrimitiveCollection(other),
        },
        other => Target::Primitive(other),
    }
}
