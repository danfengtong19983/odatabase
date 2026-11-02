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
fn each_expr(e: &Expr, visit: &mut dyn FnMut(&Expr)) {
    visit(e);
    match e {
        Expr::Literal(_) | Expr::Computed(..) | Expr::Path(_) => {}
        Expr::Not(x) | Expr::Negate(x) | Expr::Cast(x, _) => each_expr(x, visit),
        Expr::Binary(_, l, r) => {
            each_expr(l, visit);
            each_expr(r, visit);
        }
        Expr::In(l, items) => {
            each_expr(l, visit);
            items.iter().for_each(|i| each_expr(i, visit));
        }
        Expr::Call(_, args) => args.iter().for_each(|a| each_expr(a, visit)),
        Expr::Lambda { body, .. } => {
            if let Some(b) = body {
                each_expr(b, visit);
            }
        }
        Expr::IsOf(x, _) => {
            if let Some(x) = x {
                each_expr(x, visit);
            }
        }
        Expr::Function { args, .. } => args.iter().for_each(|(_, a)| each_expr(a, visit)),
    }
}

fn each_path(e: &Expr, visit: &mut dyn FnMut(&Path)) {
    each_expr(e, &mut |x| match x {
        Expr::Path(p) => visit(p),
        Expr::Lambda { source, .. } => visit(source),
        _ => {}
    });
}

/// Whether a filter depends on nothing but the row itself, so that only a change to that row
/// can change its result.
pub fn stays_on_row(e: &Expr) -> bool {
    let mut own = true;
    each_expr(e, &mut |x| own &= !matches!(x, Expr::Function { .. }));
    each_path(e, &mut |p| {
        own &= !p.steps.iter().any(|s| matches!(s, Step::Navigation(_)));
    });
    own
}

fn pipeline_filters<'o>(steps: &'o [Transformation], out: &mut Vec<&'o Expr>) {
    for t in steps {
        match t {
            Transformation::Filter(e) => out.push(e),
            Transformation::Concat { branches, .. } => {
                branches.iter().for_each(|b| pipeline_filters(b, out));
            }
            _ => {}
        }
    }
}

/// What a pipeline guarantees about every row it leaves: whether a filter applied, and on what.
/// A filter in `concat` only counts when all of its arms have it.
fn guaranteed_filters(steps: &[Transformation]) -> (bool, Vec<String>) {
    let (mut filtered, mut used) = (false, Vec::new());
    for t in steps {
        match t {
            Transformation::Filter(e) => {
                filtered = true;
                each_path(e, &mut |p| used.extend(property_path(p)));
            }
            Transformation::Concat { branches, .. } => {
                let arms: Vec<_> = branches.iter().map(|b| guaranteed_filters(b)).collect();
                filtered |= arms.iter().all(|(f, _)| *f);
                if let Some((_, first)) = arms.first() {
                    let shared = first
                        .iter()
                        .filter(|p| arms.iter().all(|(_, u)| u.contains(p)));
                    used.extend(shared.cloned());
                }
            }
            _ => {}
        }
    }
    (filtered, used)
}

/// Checks the options of a read against the restrictions of `type_name`.
pub fn check_read(model: &Model, type_name: &str, uri: &ODataUri) -> Result<(), ODataError> {
    let o = &uri.options;
    // Filters inside `$apply` must obey the same restrictions as `$filter`.
    let mut filters: Vec<&Expr> = o.filter.iter().collect();
    pipeline_filters(&o.apply, &mut filters);
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
            let (filtered, mut used) = guaranteed_filters(&o.apply);
            if needs_filter && !filtered && o.filter.is_none() {
                return Err(ODataError::bad_request(format!(
                    "{type_name} requires a $filter"
                )));
            }
            if let Some(f) = &o.filter {
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
