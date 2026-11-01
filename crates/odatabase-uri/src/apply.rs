//! `$apply` from the OData Data Aggregation extension.

use crate::UriError;
use crate::ast::*;
use crate::expr::{Scope, parse_expression};
use crate::options::{OptionContext, number, orderby, search, split_top_level};
use odatabase_edm::PrimitiveKind;
use std::sync::Arc;

/// What is addressable at one point of a pipeline.
#[derive(Clone, Debug)]
pub struct ApplyScope {
    pub entity: String,
    /// Aliases from `compute` on entities, which keep all their properties.
    pub computed: Vec<(String, ExprType)>,
    /// Set after `aggregate` or `groupby`, when only `columns` remain.
    pub aggregated: bool,
    pub columns: Vec<ApplyColumn>,
}

const NOT_IMPLEMENTED: [&str; 14] = [
    "concat",
    "nest",
    "join",
    "outerjoin",
    "expand",
    "topcount",
    "bottomcount",
    "toppercent",
    "bottompercent",
    "topsum",
    "bottomsum",
    "ancestors",
    "descendants",
    "traverse",
];

/// Parses a pipeline, returning it and the scope of the options that follow it.
pub fn parse_apply(
    ctx: &OptionContext,
    entity: &str,
    text: &str,
) -> Result<(Vec<Transformation>, Scope), UriError> {
    let mut scope = ApplyScope {
        entity: entity.to_owned(),
        computed: Vec::new(),
        aggregated: false,
        columns: Vec::new(),
    };
    let mut steps = Vec::new();
    for part in split_top_level(text, '/') {
        steps.push(transformation(ctx, &mut scope, part.trim())?);
    }
    Ok((steps, Scope::Applied(Arc::new(scope))))
}

fn transformation(
    ctx: &OptionContext,
    scope: &mut ApplyScope,
    text: &str,
) -> Result<Transformation, UriError> {
    if text == "identity" {
        return Ok(Transformation::Identity);
    }
    let (name, args) = text
        .find('(')
        .filter(|_| text.ends_with(')'))
        .map(|i| (&text[..i], &text[i + 1..text.len() - 1]))
        .ok_or_else(|| UriError::bad_request(format!("invalid transformation {text}")))?;
    let current = Scope::Applied(Arc::new(scope.clone()));
    let max = ctx.limits.max_expression_depth;
    match name {
        "filter" => {
            let e = parse_expression(ctx.model, args, current, ctx.aliases, max)?;
            if !e.ty().is_boolean() {
                return Err(UriError::bad_request("filter must be a boolean expression"));
            }
            Ok(Transformation::Filter(e))
        }
        "search" => {
            if scope.aggregated {
                return Err(UriError::bad_request("search needs entities"));
            }
            Ok(Transformation::Search(search(args)?))
        }
        "compute" => compute(ctx, scope, &current, args),
        "aggregate" => {
            let items = aggregations(ctx, scope, &current, args)?;
            scope.aggregated = true;
            scope.columns = items.iter().map(Aggregation::column).collect();
            scope.computed.clear();
            Ok(Transformation::Aggregate(items))
        }
        "groupby" => groupby(ctx, scope, &current, args),
        "orderby" => Ok(Transformation::OrderBy(orderby(ctx, &current, args)?)),
        "top" => Ok(Transformation::Top(number("top", args.trim())?)),
        "skip" => Ok(Transformation::Skip(number("skip", args.trim())?)),
        "identity" => Ok(Transformation::Identity),
        other if NOT_IMPLEMENTED.contains(&other) => Err(UriError::not_implemented(format!(
            "the {other} transformation"
        ))),
        other => Err(UriError::bad_request(format!(
            "unknown transformation {other}"
        ))),
    }
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
    if collides || scope.computed.iter().any(|(n, _)| n == alias) {
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
    let max = ctx.limits.max_expression_depth;
    let mut keys: Vec<GroupKey> = Vec::new();
    for item in split_top_level(keys_text, ',') {
        let item = item.trim();
        if item.starts_with("rollup(") || item == "$all" {
            return Err(UriError::not_implemented("rollup"));
        }
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
        if keys.iter().any(|k| k.path == path) {
            return Err(UriError::bad_request(format!("{item} is grouped twice")));
        }
        keys.push(GroupKey { path, expr, ty });
    }
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
    scope.aggregated = true;
    scope.computed.clear();
    scope.columns = keys
        .iter()
        .map(|k| ApplyColumn {
            path: k.path.clone(),
            ty: k.ty.clone(),
        })
        .collect();
    scope
        .columns
        .extend(aggregates.iter().map(Aggregation::column));
    Ok(Transformation::GroupBy { keys, aggregates })
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
        return Err(UriError::not_implemented(
            "$count of a navigation in aggregate",
        ));
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
            return Err(UriError::not_implemented("aggregation with from"));
        }
        other => {
            return Err(UriError::not_implemented(format!(
                "aggregation method {other}"
            )));
        }
    };
    let max = ctx.limits.max_expression_depth;
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
