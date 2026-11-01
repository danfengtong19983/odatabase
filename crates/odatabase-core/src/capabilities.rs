//! Enforces `Capabilities` annotations, which otherwise only describe the service.

use crate::ODataError;
use http::Method;
use odatabase_edm::{Annotation, AnnotationValue, Model};
use odatabase_uri::{Expr, ODataUri, OrderItem, Path, PathRoot, Step, Target, Transformation};

fn is_term(a: &Annotation, name: &str) -> bool {
    a.term == format!("Capabilities.{name}")
        || a.term == format!("Org.OData.Capabilities.V1.{name}")
}

/// The annotation `name` on the type, its bases, or its entity set.
fn restriction<'m>(model: &'m Model, type_name: &str, name: &str) -> Option<&'m AnnotationValue> {
    let on_type = model
        .type_chain(type_name)
        .into_iter()
        .flat_map(|t| t.annotations.iter())
        .find(|a| is_term(a, name));
    let on_set = model
        .entity_sets
        .values()
        .filter(|s| {
            model
                .type_chain(type_name)
                .iter()
                .any(|t| t.name == s.entity_type)
        })
        .flat_map(|s| s.annotations.iter())
        .find(|a| is_term(a, name));
    on_type.or(on_set).map(|a| &a.value)
}

fn field<'v>(value: &'v AnnotationValue, name: &str) -> Option<&'v AnnotationValue> {
    match value {
        AnnotationValue::Record(fields) => fields.iter().find(|(n, _)| n == name).map(|(_, v)| v),
        _ => None,
    }
}

fn flag(value: &AnnotationValue, name: &str) -> Option<bool> {
    match field(value, name) {
        Some(AnnotationValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn paths(value: &AnnotationValue, name: &str) -> Vec<String> {
    match field(value, name) {
        Some(AnnotationValue::Collection(items)) => items
            .iter()
            .filter_map(|i| match i {
                AnnotationValue::Path(p) | AnnotationValue::String(p) => Some(p.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The `/`-joined property path of an expression, when it starts at the resource.
fn property_path(p: &Path) -> Option<String> {
    if p.root != PathRoot::It {
        return None;
    }
    let names: Vec<&str> = p
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::Property(n) | Step::Navigation(n) => Some(n.as_str()),
            _ => None,
        })
        .collect();
    (!names.is_empty()).then(|| names.join("/"))
}

/// Calls `visit` on every path in `e`.
fn each_path(e: &Expr, visit: &mut dyn FnMut(&Path)) {
    match e {
        Expr::Literal(_) | Expr::Computed(..) => {}
        Expr::Path(p) => visit(p),
        Expr::Not(x) | Expr::Negate(x) | Expr::Cast(x, _) => each_path(x, visit),
        Expr::Binary(_, l, r) => {
            each_path(l, visit);
            each_path(r, visit);
        }
        Expr::In(l, items) => {
            each_path(l, visit);
            items.iter().for_each(|i| each_path(i, visit));
        }
        Expr::Call(_, args) => args.iter().for_each(|a| each_path(a, visit)),
        Expr::Lambda { source, body, .. } => {
            visit(source);
            if let Some(b) = body {
                each_path(b, visit);
            }
        }
        Expr::IsOf(x, _) => {
            if let Some(x) = x {
                each_path(x, visit);
            }
        }
        Expr::Function { args, .. } => args.iter().for_each(|(_, a)| each_path(a, visit)),
    }
}

/// Checks the options of a read against the restrictions of `type_name`.
pub fn check_read(model: &Model, type_name: &str, uri: &ODataUri) -> Result<(), ODataError> {
    let o = &uri.options;
    // Filters inside `$apply` must obey the same restrictions as `$filter`.
    let filters: Vec<&Expr> = o
        .filter
        .iter()
        .chain(o.apply.iter().filter_map(|t| match t {
            Transformation::Filter(e) => Some(e),
            _ => None,
        }))
        .collect();
    if let Some(r) = restriction(model, type_name, "FilterRestrictions") {
        if !filters.is_empty() && flag(r, "Filterable") == Some(false) {
            return Err(ODataError::bad_request(format!(
                "{type_name} cannot be filtered"
            )));
        }
        let keyed = matches!(uri.target(), Target::Entity(_) | Target::Count);
        if !keyed || matches!(uri.target(), Target::Count) {
            let required = paths(r, "RequiredProperties");
            let needs_filter = flag(r, "RequiresFilter") == Some(true) || !required.is_empty();
            if needs_filter && filters.is_empty() {
                return Err(ODataError::bad_request(format!(
                    "{type_name} requires a $filter"
                )));
            }
            let mut used = Vec::new();
            for f in &filters {
                each_path(f, &mut |p| used.extend(property_path(p)));
            }
            for name in required {
                if !used.contains(&name) {
                    return Err(ODataError::bad_request(format!("$filter must use {name}")));
                }
            }
        }
        for f in &filters {
            let banned = paths(r, "NonFilterableProperties");
            let mut violation = None;
            each_path(f, &mut |p| {
                if let Some(name) = property_path(p)
                    && banned.contains(&name)
                {
                    violation = Some(name);
                }
            });
            if let Some(name) = violation {
                return Err(ODataError::bad_request(format!(
                    "{name} cannot be filtered"
                )));
            }
        }
    }
    if let Some(r) = restriction(model, type_name, "SortRestrictions") {
        if !o.orderby.is_empty() && flag(r, "Sortable") == Some(false) {
            return Err(ODataError::bad_request(format!(
                "{type_name} cannot be sorted"
            )));
        }
        let banned = paths(r, "NonSortableProperties");
        for OrderItem { expr, .. } in &o.orderby {
            let mut violation = None;
            each_path(expr, &mut |p| {
                if let Some(name) = property_path(p)
                    && banned.contains(&name)
                {
                    violation = Some(name);
                }
            });
            if let Some(name) = violation {
                return Err(ODataError::bad_request(format!("{name} cannot be sorted")));
            }
        }
    }
    if let Some(r) = restriction(model, type_name, "CountRestrictions")
        && flag(r, "Countable") == Some(false)
        && (o.count || *uri.target() == Target::Count)
    {
        return Err(ODataError::bad_request(format!(
            "{type_name} cannot be counted"
        )));
    }
    if let Some(r) = restriction(model, type_name, "ExpandRestrictions")
        && flag(r, "Expandable") == Some(false)
        && !o.expand.is_empty()
    {
        return Err(ODataError::bad_request(format!(
            "{type_name} cannot be expanded"
        )));
    }
    if let Some(AnnotationValue::Bool(false)) = restriction(model, type_name, "TopSupported")
        && o.top.is_some()
    {
        return Err(ODataError::not_implemented(format!("$top on {type_name}")));
    }
    if let Some(AnnotationValue::Bool(false)) = restriction(model, type_name, "SkipSupported")
        && o.skip.is_some()
    {
        return Err(ODataError::not_implemented(format!("$skip on {type_name}")));
    }
    Ok(())
}

/// Checks that `type_name` allows the change `method` makes.
pub fn check_write(model: &Model, type_name: &str, method: &Method) -> Result<(), ODataError> {
    let (term, field_name, verb) = match *method {
        Method::POST => ("InsertRestrictions", "Insertable", "created"),
        Method::PATCH | Method::PUT => ("UpdateRestrictions", "Updatable", "updated"),
        Method::DELETE => ("DeleteRestrictions", "Deletable", "deleted"),
        _ => return Ok(()),
    };
    if let Some(r) = restriction(model, type_name, term)
        && flag(r, field_name) == Some(false)
    {
        return Err(ODataError::new(
            405,
            format!("{type_name} cannot be {verb}"),
        ));
    }
    Ok(())
}
