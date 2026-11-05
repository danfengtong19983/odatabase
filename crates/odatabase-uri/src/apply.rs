//! `$apply` from the OData Data Aggregation extension.

use crate::UriError;
use crate::ast::*;
use crate::expr::{Scope, parse_expression, property_type};
use crate::options::{OptionContext, number, orderby, search, split_top_level};
use odatabase_edm::{Model, PrimitiveKind, TypeKind};
use std::sync::Arc;

/// What is addressable at one point of a pipeline.
#[derive(Clone, Debug)]
pub struct ApplyScope {
    pub entity: String,
    /// Aliases from `compute` on entities, which keep all their properties.
    pub computed: Vec<(String, ExprType)>,
    /// Set after an aggregation, when only `columns` remain.
    pub aggregated: bool,
    pub columns: Vec<ApplyColumn>,
    /// Aliases from `join` and `outerjoin`, which act like lambda variables.
    pub joins: Vec<(String, ExprType)>,
}

/// These need hierarchy annotations or nested results, which the model does not have.
const NOT_IMPLEMENTED: [&str; 4] = ["nest", "ancestors", "descendants", "traverse"];

/// The transformation names listed in `Aggregation.ApplySupported`.
pub const SUPPORTED: [&str; 19] = [
    "aggregate",
    "bottomcount",
    "bottompercent",
    "bottomsum",
    "compute",
    "concat",
    "expand",
    "filter",
    "groupby",
    "identity",
    "join",
    "orderby",
    "outerjoin",
    "search",
    "skip",
    "top",
    "topcount",
    "toppercent",
    "topsum",
];

/// A parsed pipeline, with what it leaves in scope for the options that follow it.
pub struct ParsedApply {
    pub steps: Vec<Transformation>,
    pub scope: Scope,
    /// From `expand` transformations, which act like `$expand`.
    pub expand: Vec<ExpandItem>,
}

pub fn parse_apply(ctx: &OptionContext, entity: &str, text: &str) -> Result<ParsedApply, UriError> {
    let mut scope = ApplyScope {
        entity: entity.to_owned(),
        computed: Vec::new(),
        aggregated: false,
        columns: Vec::new(),
        joins: Vec::new(),
    };
    let mut steps = Vec::new();
    let mut expand = Vec::new();
    pipeline(ctx, &mut scope, text, &mut steps, &mut expand)?;
    if !scope.joins.is_empty() && !scope.aggregated {
        return Err(UriError::not_implemented(
            "join without a later aggregation",
        ));
    }
    if !expand.is_empty() && scope.aggregated {
        return Err(UriError::not_implemented(
            "the expand transformation before an aggregation",
        ));
    }
    Ok(ParsedApply {
        steps,
        scope: Scope::Applied(Arc::new(scope)),
        expand,
    })
}

fn pipeline(
    ctx: &OptionContext,
    scope: &mut ApplyScope,
    text: &str,
    steps: &mut Vec<Transformation>,
    expand: &mut Vec<ExpandItem>,
) -> Result<(), UriError> {
    for part in split_top_level(text, '/') {
        transformation(ctx, scope, part.trim(), steps, expand)?;
    }
    Ok(())
}

fn transformation(
    ctx: &OptionContext,
    scope: &mut ApplyScope,
    text: &str,
    steps: &mut Vec<Transformation>,
    expand: &mut Vec<ExpandItem>,
) -> Result<(), UriError> {
    if text == "identity" {
        steps.push(Transformation::Identity);
        return Ok(());
    }
    let (name, args) = text
        .find('(')
        .filter(|_| text.ends_with(')'))
        .map(|i| (&text[..i], &text[i + 1..text.len() - 1]))
        .ok_or_else(|| UriError::bad_request(format!("invalid transformation {text}")))?;
    let current = Scope::Applied(Arc::new(scope.clone()));
    let max = ctx.limits.max_expression_depth;
    let step = match name {
        "filter" => {
            let e = parse_expression(ctx.model, args, current, ctx.aliases, max)?;
            if !e.ty().is_boolean() {
                return Err(UriError::bad_request("filter must be a boolean expression"));
            }
            Transformation::Filter(e)
        }
        "search" => {
            if scope.aggregated {
                return Err(UriError::bad_request("search needs entities"));
            }
            Transformation::Search(search(args)?)
        }
        "compute" => compute(ctx, scope, &current, args)?,
        "aggregate" => {
            if nested_from(args) {
                return Err(UriError::not_implemented("from with more than one level"));
            }
            if let Some(rewritten) = rewrite_from(args) {
                for part in rewritten {
                    transformation(ctx, scope, &part, steps, expand)?;
                }
                return Ok(());
            }
            let items = aggregations(ctx, scope, &current, args)?;
            scope.aggregated = true;
            scope.columns = items.iter().map(Aggregation::column).collect();
            scope.computed.clear();
            scope.joins.clear();
            Transformation::Aggregate(items)
        }
        "groupby" => groupby(ctx, scope, &current, args)?,
        "orderby" => Transformation::OrderBy(orderby(ctx, &current, args)?),
        "top" => Transformation::Top(number("top", args.trim())?),
        "skip" => Transformation::Skip(number("skip", args.trim())?),
        "concat" => concat(ctx, scope, args)?,
        "topcount" | "bottomcount" | "topsum" | "bottomsum" | "toppercent" | "bottompercent" => {
            rank(ctx, &current, name, args)?
        }
        "join" | "outerjoin" => join(ctx, scope, &current, name, args)?,
        "expand" => {
            if scope.aggregated {
                return Err(UriError::bad_request("expand needs entities"));
            }
            expand.push(expand_step(ctx, &scope.entity, args)?);
            return Ok(());
        }
        other if NOT_IMPLEMENTED.contains(&other) => {
            return Err(UriError::not_implemented(format!(
                "the {other} transformation"
            )));
        }
        other => {
            return Err(UriError::bad_request(format!(
                "unknown transformation {other}"
            )));
        }
    };
    steps.push(step);
    Ok(())
}

/// Whether an aggregate expression has `from` more than once, as in
/// `E with a from P with b from Q with c as X`.
fn nested_from(args: &str) -> bool {
    split_top_level(args, ',').into_iter().any(|item| {
        let body = split_keyword(item.trim(), "as").map_or(item.trim(), |(body, _)| body);
        split_keyword(body, "from").is_some_and(|(inner, _)| split_keyword(inner, "from").is_some())
    })
}

/// `E with a from P with b as X` first aggregates by `P`, then aggregates those results.
fn rewrite_from(args: &str) -> Option<[String; 2]> {
    let [item] = split_top_level(args, ',').try_into().ok()?;
    let (body, alias) = split_keyword(item.trim(), "as")?;
    let (inner, outer) = split_keyword(body, "from")?;
    if split_keyword(inner, "from").is_some() {
        return None;
    }
    let (property, method) = match split_keyword(outer, "with") {
        Some((p, m)) => (p.trim(), m.trim().to_owned()),
        None => {
            let (_, m) = split_keyword(inner, "with")?;
            (outer.trim(), m.trim().to_owned())
        }
    };
    let temporary = "_from";
    Some([
        format!(
            "groupby(({property}),aggregate({} as {temporary}))",
            inner.trim()
        ),
        format!("aggregate({temporary} with {method} as {})", alias.trim()),
    ])
}

/// Finds the last whole `word` outside quotes, splitting the text around it.
fn split_keyword<'t>(text: &'t str, word: &str) -> Option<(&'t str, &'t str)> {
    let is_gap = |c: u8| matches!(c, b' ' | b'+' | b'\t');
    let bytes = text.as_bytes();
    let (mut quoted, mut found) = (false, None);
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\'' {
            quoted = !quoted;
        }
        let end = i + word.len();
        if !quoted
            && i > 0
            && end < bytes.len()
            && is_gap(bytes[i - 1])
            && is_gap(bytes[end])
            && text.get(i..end) == Some(word)
        {
            found = Some(i);
        }
    }
    let i = found?;
    Some((&text[..i - 1], &text[i + word.len() + 1..]))
}

fn check_alias(scope: &ApplyScope, ctx: &OptionContext, alias: &str) -> Result<(), UriError> {
    let valid = alias
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && alias.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !valid {
        return Err(UriError::bad_request(format!("invalid alias {alias}")));
    }
    let collides = if scope.aggregated {
        scope
            .columns
            .iter()
            .any(|c| c.path.first().is_some_and(|f| f == alias))
    } else {
        ctx.model.property(&scope.entity, alias).is_some()
            || ctx.model.navigation(&scope.entity, alias).is_some()
    };
    let taken = scope
        .computed
        .iter()
        .chain(&scope.joins)
        .any(|(n, _)| n == alias);
    if collides || taken {
        return Err(UriError::bad_request(format!(
            "alias {alias} is already in use"
        )));
    }
    Ok(())
}

fn compute(
    ctx: &OptionContext,
    scope: &mut ApplyScope,
    current: &Scope,
    args: &str,
) -> Result<Transformation, UriError> {
    let mut items = Vec::new();
    let max = ctx.limits.max_expression_depth;
    for part in split_top_level(args, ',') {
        let (text, alias) = split_keyword(part.trim(), "as")
            .ok_or_else(|| UriError::bad_request("compute needs: expression as alias"))?;
        let alias = alias.trim().to_owned();
        check_alias(scope, ctx, &alias)?;
        if items.iter().any(|i: &ComputeItem| i.alias == alias) {
            return Err(UriError::bad_request(format!(
                "alias {alias} is already in use"
            )));
        }
        let expr = parse_expression(ctx.model, text, current.clone(), ctx.aliases, max)?;
        let ty = expr.ty();
        require_scalar(&alias, &ty)?;
        items.push(ComputeItem { expr, alias, ty });
    }
    for item in &items {
        if scope.aggregated {
            scope.columns.push(ApplyColumn {
                path: vec![item.alias.clone()],
                ty: item.ty.clone(),
            });
        } else {
            scope.computed.push((item.alias.clone(), item.ty.clone()));
        }
    }
    Ok(Transformation::Compute(items))
}

fn require_scalar(name: &str, ty: &ExprType) -> Result<(), UriError> {
    match ty {
        ExprType::Primitive(k) if !k.is_geo() => Ok(()),
        ExprType::Enum(_) => Ok(()),
        _ => Err(UriError::bad_request(format!(
            "{name} must be a primitive value"
        ))),
    }
}

/// The columns entities contribute to `concat`: every stored scalar, with embedded types nested.
fn entity_columns(model: &Model, scope: &ApplyScope) -> Vec<ApplyColumn> {
    fn add(
        model: &Model,
        p: &odatabase_edm::Property,
        prefix: &[String],
        out: &mut Vec<ApplyColumn>,
    ) {
        let mut path = prefix.to_vec();
        path.push(p.name.clone());
        match &p.kind {
            TypeKind::Complex(t) if p.collection.is_none() => {
                for inner in model.complex_type(t).iter().flat_map(|c| &c.properties) {
                    add(model, inner, &path, out);
                }
            }
            _ if p.is_stored_scalar() => {
                out.push(ApplyColumn {
                    path,
                    ty: property_type(p),
                });
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for p in model.all_properties(&scope.entity) {
        add(model, p, &[], &mut out);
    }
    for (name, ty) in &scope.computed {
        out.push(ApplyColumn {
            path: vec![name.clone()],
            ty: ty.clone(),
        });
    }
    out
}

fn merge_columns(into: &mut Vec<ApplyColumn>, columns: Vec<ApplyColumn>) -> Result<(), UriError> {
    for c in columns {
        match into.iter().find(|o| o.path == c.path) {
            Some(o) if o.ty != c.ty => {
                return Err(UriError::bad_request(format!(
                    "{} has different types in concat",
                    c.path.join("/")
                )));
            }
            Some(_) => {}
            None => into.push(c),
        }
    }
    Ok(())
}

fn concat(
    ctx: &OptionContext,
    scope: &mut ApplyScope,
    args: &str,
) -> Result<Transformation, UriError> {
    let arms = split_top_level(args, ',');
    if arms.len() < 2 {
        return Err(UriError::bad_request("concat needs at least two pipelines"));
    }
    let (mut branches, mut columns) = (Vec::with_capacity(arms.len()), Vec::new());
    for arm in arms {
        let mut inner = scope.clone();
        let (mut steps, mut expand) = (Vec::new(), Vec::new());
        pipeline(ctx, &mut inner, arm.trim(), &mut steps, &mut expand)?;
        if !expand.is_empty() {
            return Err(UriError::not_implemented(
                "the expand transformation in concat",
            ));
        }
        if !inner.joins.is_empty() && !inner.aggregated {
            return Err(UriError::not_implemented(
                "join without a later aggregation",
            ));
        }
        let arm_columns = if inner.aggregated {
            inner.columns
        } else {
            entity_columns(ctx.model, &inner)
        };
        merge_columns(&mut columns, arm_columns)?;
        branches.push(steps);
    }
    scope.aggregated = true;
    scope.computed.clear();
    scope.joins.clear();
    scope.columns = columns.clone();
    Ok(Transformation::Concat { branches, columns })
}

fn rank(
    ctx: &OptionContext,
    current: &Scope,
    name: &str,
    args: &str,
) -> Result<Transformation, UriError> {
    let (amount_text, value_text) = match split_top_level(args, ',').as_slice() {
        [a, v] => (a.trim().to_owned(), v.trim().to_owned()),
        _ => {
            return Err(UriError::bad_request(format!(
                "{name} takes an amount and a value"
            )));
        }
    };
    let max = ctx.limits.max_expression_depth;
    let amount = parse_expression(ctx.model, &amount_text, Scope::None, ctx.aliases, max)?;
    let value = parse_expression(ctx.model, &value_text, current.clone(), ctx.aliases, max)?;
    let numeric = |t: &ExprType| matches!(t, ExprType::Primitive(k) if k.is_numeric());
    let kind = match name.trim_start_matches("top").trim_start_matches("bottom") {
        "count" => RankKind::Count,
        "sum" => RankKind::Sum,
        _ => RankKind::Percent,
    };
    if !matches!(amount, Expr::Literal(_)) || !numeric(&amount.ty()) {
        return Err(UriError::bad_request(format!(
            "{name} needs a literal amount"
        )));
    }
    if kind == RankKind::Count
        && !matches!(&amount, Expr::Literal(v) if v.as_i64().is_some_and(|n| n >= 0))
    {
        return Err(UriError::bad_request(format!(
            "{name} needs a whole number"
        )));
    }
    if kind == RankKind::Count {
        require_scalar(name, &value.ty())?;
    } else if !numeric(&value.ty()) {
        return Err(UriError::bad_request(format!(
            "{name} needs a number to add up"
        )));
    }
    Ok(Transformation::Rank {
        kind,
        top: name.starts_with("top"),
        amount,
        value,
    })
}

fn join(
    ctx: &OptionContext,
    scope: &mut ApplyScope,
    current: &Scope,
    name: &str,
    args: &str,
) -> Result<Transformation, UriError> {
    if scope.aggregated {
        return Err(UriError::bad_request(format!("{name} needs entities")));
    }
    let (text, alias) = split_keyword(args.trim(), "as")
        .ok_or_else(|| UriError::bad_request(format!("{name} needs: path as alias")))?;
    let alias = alias.trim().to_owned();
    check_alias(scope, ctx, &alias)?;
    let max = ctx.limits.max_expression_depth;
    let parsed = parse_expression(ctx.model, text, current.clone(), ctx.aliases, max)?;
    let Expr::Path(path) = parsed else {
        return Err(UriError::bad_request(format!(
            "{name} needs a navigation path"
        )));
    };
    let element = match &path.ty {
        ExprType::Collection(inner) => (**inner).clone(),
        other => other.clone(),
    };
    if !matches!(path.ty, ExprType::Collection(_) | ExprType::Entity(_)) {
        return Err(UriError::bad_request(format!(
            "{name} needs a navigation path"
        )));
    }
    let only_navigation = path.steps.iter().all(|s| matches!(s, Step::Navigation(_)));
    if !only_navigation || !matches!(element, ExprType::Entity(_)) {
        return Err(UriError::not_implemented(format!(
            "{name} on anything but navigation"
        )));
    }
    scope.joins.push((alias.clone(), element));
    Ok(Transformation::Join {
        path,
        alias,
        outer: name == "outerjoin",
    })
}

/// `expand(Nav,filter(...),expand(...))`.
fn expand_step(ctx: &OptionContext, owner: &str, args: &str) -> Result<ExpandItem, UriError> {
    let parts = split_top_level(args, ',');
    let name = parts[0].trim();
    let nav = ctx
        .model
        .navigation(owner, name)
        .ok_or_else(|| UriError::bad_request(format!("unknown navigation property {name}")))?;
    let mut item = ExpandItem {
        prefix: Vec::new(),
        navigation: nav.name.clone(),
        target: nav.target.clone(),
        collection: nav.collection,
        options: QueryOptions::default(),
        levels: None,
        count_only: false,
        ref_only: false,
        from_star: false,
    };
    let scope = Scope::Entity(nav.target.clone());
    for part in &parts[1..] {
        let part = part.trim();
        let max = ctx.limits.max_expression_depth;
        if let Some(text) = part
            .strip_prefix("filter(")
            .and_then(|p| p.strip_suffix(')'))
        {
            let e = parse_expression(ctx.model, text, scope.clone(), ctx.aliases, max)?;
            if !e.ty().is_boolean() {
                return Err(UriError::bad_request("filter must be a boolean expression"));
            }
            item.options.filter = Some(e);
        } else if let Some(text) = part
            .strip_prefix("expand(")
            .and_then(|p| p.strip_suffix(')'))
        {
            item.options
                .expand
                .push(expand_step(ctx, &nav.target, text)?);
        } else {
            return Err(UriError::not_implemented(format!("{part} inside expand")));
        }
    }
    Ok(item)
}

/// The sets of keys of a `groupby` list: `rollup` expands to one set per level.
fn grouping_sets(list: &str) -> Vec<Vec<String>> {
    let mut sets: Vec<Vec<String>> = vec![Vec::new()];
    for item in split_top_level(list, ',') {
        let item = item.trim();
        let Some(inner) = item
            .strip_prefix("rollup(")
            .and_then(|i| i.strip_suffix(')'))
        else {
            sets.iter_mut().for_each(|s| s.push(item.to_owned()));
            continue;
        };
        let names: Vec<&str> = split_top_level(inner, ',')
            .into_iter()
            .map(str::trim)
            .collect();
        let include_total = names.first() == Some(&"$all");
        let names = if include_total {
            &names[1..]
        } else {
            &names[..]
        };
        let lowest = usize::from(!include_total);
        let mut levels: Vec<Vec<String>> = Vec::new();
        for n in (lowest..=names.len()).rev() {
            levels.push(names[..n].iter().map(|s| (*s).to_owned()).collect());
        }
        sets = sets
            .iter()
            .flat_map(|s| {
                levels.iter().map(move |l| {
                    let mut combined = s.clone();
                    combined.extend(l.iter().cloned());
                    combined
                })
            })
            .collect();
    }
    sets
}

fn group_keys(
    ctx: &OptionContext,
    current: &Scope,
    keys: &[String],
) -> Result<Vec<GroupKey>, UriError> {
    let max = ctx.limits.max_expression_depth;
    let mut out: Vec<GroupKey> = Vec::new();
    for item in keys {
        let expr = parse_expression(ctx.model, item, current.clone(), ctx.aliases, max)?;
        if !matches!(expr, Expr::Path(_) | Expr::Computed(..)) {
            return Err(UriError::bad_request(format!(
                "{item} is not a property path"
            )));
        }
        let ty = expr.ty();
        require_scalar(item, &ty)?;
        let path: Vec<String> = item
            .split('/')
            .filter(|s| !s.contains('.'))
            .map(str::to_owned)
            .collect();
        if out.iter().any(|k| k.path == path) {
            return Err(UriError::bad_request(format!("{item} is grouped twice")));
        }
        out.push(GroupKey { path, expr, ty });
    }
    Ok(out)
}

fn groupby(
    ctx: &OptionContext,
    scope: &mut ApplyScope,
    current: &Scope,
    args: &str,
) -> Result<Transformation, UriError> {
    let parts = split_top_level(args, ',');
    let keys_text = parts
        .first()
        .map(|p| p.trim())
        .and_then(|p| p.strip_prefix('('))
        .and_then(|p| p.strip_suffix(')'))
        .ok_or_else(|| UriError::bad_request("groupby needs a list of properties"))?;
    let aggregates = match parts.len() {
        1 => Vec::new(),
        2 => {
            let inner = parts[1]
                .trim()
                .strip_prefix("aggregate(")
                .and_then(|p| p.strip_suffix(')'))
                .ok_or_else(|| {
                    UriError::not_implemented("groupby supports only aggregate as a nested step")
                })?;
            aggregations(ctx, scope, current, inner)?
        }
        _ => {
            return Err(UriError::bad_request(
                "groupby takes properties and one step",
            ));
        }
    };
    let sets = grouping_sets(keys_text);
    let mut steps = Vec::with_capacity(sets.len());
    let mut columns: Vec<ApplyColumn> = Vec::new();
    for set in &sets {
        let keys = group_keys(ctx, current, set)?;
        let mut set_columns: Vec<ApplyColumn> = keys
            .iter()
            .map(|k| ApplyColumn {
                path: k.path.clone(),
                ty: k.ty.clone(),
            })
            .collect();
        set_columns.extend(aggregates.iter().map(Aggregation::column));
        merge_columns(&mut columns, set_columns)?;
        steps.push(vec![Transformation::GroupBy {
            keys,
            aggregates: aggregates.clone(),
        }]);
    }
    scope.aggregated = true;
    scope.computed.clear();
    scope.joins.clear();
    scope.columns = columns.clone();
    Ok(match steps.len() {
        1 => steps.remove(0).remove(0),
        _ => Transformation::Concat {
            branches: steps,
            columns,
        },
    })
}

fn aggregations(
    ctx: &OptionContext,
    scope: &ApplyScope,
    current: &Scope,
    args: &str,
) -> Result<Vec<Aggregation>, UriError> {
    let mut items: Vec<Aggregation> = Vec::new();
    for part in split_top_level(args, ',') {
        let item = aggregation(ctx, scope, current, part.trim())?;
        if items.iter().any(|i| i.alias == item.alias) {
            return Err(UriError::bad_request(format!(
                "alias {} is already in use",
                item.alias
            )));
        }
        items.push(item);
    }
    Ok(items)
}

fn aggregation(
    ctx: &OptionContext,
    scope: &ApplyScope,
    current: &Scope,
    text: &str,
) -> Result<Aggregation, UriError> {
    use PrimitiveKind as P;
    let (body, alias) = split_keyword(text, "as")
        .ok_or_else(|| UriError::bad_request(format!("{text} needs an alias")))?;
    let (body, alias) = (body.trim(), alias.trim().to_owned());
    check_alias(scope, ctx, &alias)?;
    let max = ctx.limits.max_expression_depth;
    if body == "$count" {
        let ty = ExprType::Primitive(P::Int64);
        return Ok(Aggregation {
            alias,
            method: AggregateMethod::Count,
            expr: None,
            ty,
        });
    }
    if body.ends_with("/$count") {
        // The related rows of every input row, added up.
        let expr = parse_expression(ctx.model, body, current.clone(), ctx.aliases, max)?;
        let ty = ExprType::Primitive(P::Decimal);
        return Ok(Aggregation {
            alias,
            method: AggregateMethod::Sum,
            expr: Some(expr),
            ty,
        });
    }
    let (expr_text, method) = split_keyword(body, "with")
        .ok_or_else(|| UriError::bad_request(format!("{text} needs: expression with method")))?;
    let method_name = method.trim();
    let method = match method_name {
        "sum" => AggregateMethod::Sum,
        "min" => AggregateMethod::Min,
        "max" => AggregateMethod::Max,
        "average" => AggregateMethod::Average,
        "countdistinct" => AggregateMethod::CountDistinct,
        other if other.split_whitespace().count() > 1 || other.contains('+') => {
            return Err(UriError::not_implemented("this form of from"));
        }
        other => {
            return Err(UriError::not_implemented(format!(
                "aggregation method {other}"
            )));
        }
    };
    let expr = parse_expression(ctx.model, expr_text, current.clone(), ctx.aliases, max)?;
    let input = expr.ty();
    let numeric = matches!(&input, ExprType::Primitive(k) if k.is_numeric());
    let float = matches!(&input, ExprType::Primitive(P::Single | P::Double));
    let ty = match method {
        AggregateMethod::Sum if float => ExprType::Primitive(P::Double),
        AggregateMethod::Sum if numeric => ExprType::Primitive(P::Decimal),
        AggregateMethod::Average if input == ExprType::Primitive(P::Decimal) => input.clone(),
        AggregateMethod::Average if numeric => ExprType::Primitive(P::Double),
        AggregateMethod::Sum | AggregateMethod::Average => {
            return Err(UriError::bad_request(format!(
                "{method_name} needs a number"
            )));
        }
        AggregateMethod::Min | AggregateMethod::Max => {
            if input == ExprType::Primitive(P::Boolean) {
                return Err(UriError::bad_request("booleans have no order"));
            }
            require_scalar(&alias, &input)?;
            input.clone()
        }
        AggregateMethod::CountDistinct => {
            require_scalar(&alias, &input)?;
            ExprType::Primitive(P::Int64)
        }
        AggregateMethod::Count => unreachable!("handled above"),
    };
    Ok(Aggregation {
        alias,
        method,
        expr: Some(expr),
        ty,
    })
}
