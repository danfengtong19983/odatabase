//! System query options.

use crate::UriError;
use crate::apply::parse_apply;
use crate::ast::*;
use crate::expr::{Aliases, Scope, parse_expression, property_type};
use crate::path::Limits;
use odatabase_edm::Model;

/// Everything needed to parse options at one level of a request.
pub struct OptionContext<'a> {
    pub model: &'a Model,
    pub aliases: &'a Aliases,
    pub limits: Limits,
    pub expand_depth: u32,
}

/// Splits `text` on `sep` outside parentheses and quotes.
pub fn split_top_level(text: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start) = (0i32, false, 0);
    for (i, c) in text.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            c if c == sep && !quoted && depth == 0 => {
                parts.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

pub fn apply_option(
    ctx: &OptionContext,
    scope: &Scope,
    o: &mut QueryOptions,
    name: &str,
    value: &str,
) -> Result<(), UriError> {
    let max = ctx.limits.max_expression_depth;
    match name {
        "$filter" => {
            let e = parse_expression(ctx.model, value, scope.clone(), ctx.aliases, max)?;
            if !e.ty().is_boolean() {
                return Err(UriError::bad_request(
                    "$filter must be a boolean expression",
                ));
            }
            o.filter = Some(e);
        }
        "$orderby" => o.orderby = orderby(ctx, scope, value)?,
        "$select" => {
            let mut nested = Vec::new();
            o.select = Some(select(ctx, scope, value, &mut nested)?);
            o.property_options = nested;
        }
        "$expand" => o.expand = expand(ctx, scope, value)?,
        "$top" => o.top = Some(number(name, value)?),
        "$skip" => o.skip = Some(number(name, value)?),
        "$count" => {
            o.count = match value {
                "true" => true,
                "false" => false,
                _ => return Err(UriError::bad_request("$count must be true or false")),
            }
        }
        "$search" => o.search = Some(search(value)?),
        "$skiptoken" => o.skiptoken = Some(value.to_owned()),
        "$deltatoken" => o.deltatoken = Some(value.to_owned()),
        "$format" => o.format = Some(value.to_owned()),
        // The target of `DELETE .../$ref?$id=...`, read by the write path.
        "$id" => o.custom.push((name.to_owned(), value.to_owned())),
        "$schemaversion" => o.schemaversion = Some(value.to_owned()),
        "$index" => {
            o.index = Some(
                value
                    .parse()
                    .map_err(|_| UriError::bad_request("$index must be an integer"))?,
            );
        }
        "$apply" | "$compute" => {
            return Err(UriError::not_implemented(format!(
                "{name} is not supported"
            )));
        }
        other => {
            return Err(UriError::bad_request(format!(
                "unknown query option {other}"
            )));
        }
    }
    Ok(())
}

/// Databases take limits as signed 64-bit numbers, so larger values are invalid here.
pub fn number(name: &str, value: &str) -> Result<u64, UriError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| i64::try_from(*n).is_ok())
        .ok_or_else(|| UriError::bad_request(format!("{name} must be a non-negative integer")))
}

pub fn orderby(
    ctx: &OptionContext,
    scope: &Scope,
    value: &str,
) -> Result<Vec<OrderItem>, UriError> {
    let mut items = Vec::new();
    for part in split_top_level(value, ',') {
        let part = part.trim_matches([' ', '+']);
        let (text, descending) = if let Some(t) = strip_word(part, "desc") {
            (t, true)
        } else if let Some(t) = strip_word(part, "asc") {
            (t, false)
        } else {
            (part, false)
        };
        let max = ctx.limits.max_expression_depth;
        let expr = parse_expression(ctx.model, text, scope.clone(), ctx.aliases, max)?;
        match expr.ty() {
            ExprType::Primitive(_) | ExprType::Enum(_) | ExprType::Null => {}
            other => {
                return Err(UriError::bad_request(format!("cannot order by {other:?}")));
            }
        }
        items.push(OrderItem { expr, descending });
    }
    Ok(items)
}

/// Removes a trailing `asc`/`desc` keyword separated by a space or a `+`.
fn strip_word<'t>(text: &'t str, word: &str) -> Option<&'t str> {
    let head = text.strip_suffix(word)?;
    head.ends_with([' ', '+'])
        .then(|| head.trim_end_matches([' ', '+']))
}

/// `$compute` after `$apply`. Each item is a step, so that it sees the ones before it.
pub fn pipeline_text(apply: Option<&str>, compute: Option<&str>) -> Option<String> {
    let computed = compute.map(|c| {
        let steps: Vec<String> = split_top_level(c, ',')
            .iter()
            .map(|item| format!("compute({item})"))
            .collect();
        steps.join("/")
    });
    match (apply, computed) {
        (Some(a), Some(c)) => Some(format!("{a}/{c}")),
        (Some(a), None) => Some(a.to_owned()),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

/// The scope a property path leads to, and whether it is a collection.
fn path_scope(
    ctx: &OptionContext,
    scope: &Scope,
    steps: &[String],
) -> Result<(Scope, bool), UriError> {
    let invalid = || UriError::bad_request(format!("{} takes no options", steps.join("/")));
    let (mut current, mut collection) = (scope.clone(), false);
    for step in steps {
        let p = match &current {
            Scope::Entity(t) => ctx.model.property(t, step),
            Scope::Applied(a) if !a.aggregated => ctx.model.property(&a.entity, step),
            Scope::Complex(c) => ctx
                .model
                .complex_type(c)
                .and_then(|t| t.properties.iter().find(|p| p.name == *step)),
            _ => None,
        };
        let p = p.ok_or_else(invalid)?;
        collection = p.collection.is_some();
        current = match property_type(p) {
            ExprType::Complex(c) => Scope::Complex(c),
            ExprType::Collection(inner) => match *inner {
                ExprType::Complex(c) => Scope::Complex(c),
                other => Scope::Value(other),
            },
            other => Scope::Value(other),
        };
    }
    Ok((current, collection))
}

fn select_options(
    ctx: &OptionContext,
    scope: &Scope,
    steps: &[String],
    text: &str,
    items: &mut Vec<SelectItem>,
    nested: &mut Vec<(String, QueryOptions)>,
) -> Result<(), UriError> {
    let invalid = || UriError::bad_request(format!("{} takes no options", steps.join("/")));
    let (inner, collection) = path_scope(ctx, scope, steps)?;
    if collection {
        if steps.len() != 1 {
            return Err(invalid());
        }
        let mut options = QueryOptions::default();
        for part in split_top_level(text, ';') {
            let (name, value) = part
                .split_once('=')
                .ok_or_else(|| UriError::bad_request(format!("invalid option {part}")))?;
            let name = name.trim();
            if !matches!(
                name,
                "$filter" | "$orderby" | "$top" | "$skip" | "$count" | "$select" | "$search"
            ) {
                return Err(UriError::bad_request(format!(
                    "{name} is not allowed in $select"
                )));
            }
            apply_option(ctx, &inner, &mut options, name, value)?;
        }
        nested.push((steps[0].clone(), options));
        items.push(SelectItem::Path(steps.to_vec()));
        return Ok(());
    }
    if !matches!(inner, Scope::Complex(_)) {
        return Err(invalid());
    }
    let value = text.trim().strip_prefix("$select=").ok_or_else(invalid)?;
    for item in select(ctx, &inner, value, nested)? {
        match item {
            SelectItem::Star => items.push(SelectItem::Path(steps.to_vec())),
            SelectItem::Path(rest) => {
                items.push(SelectItem::Path([steps, &rest[..]].concat()));
            }
        }
    }
    Ok(())
}

fn select(
    ctx: &OptionContext,
    scope: &Scope,
    value: &str,
    nested: &mut Vec<(String, QueryOptions)>,
) -> Result<Vec<SelectItem>, UriError> {
    let mut items = Vec::new();
    for raw in split_top_level(value, ',') {
        let raw = raw.trim();
        if raw == "*" {
            items.push(SelectItem::Star);
            continue;
        }
        if raw.is_empty() {
            return Err(UriError::bad_request("empty $select item"));
        }
        // `Namespace.*` asks for the operations of a type, which are metadata rather than data.
        if ctx.model.qualified("*") == raw {
            continue;
        }
        let (path, options) = match raw.find('(') {
            Some(i) if raw.ends_with(')') => (&raw[..i], Some(&raw[i + 1..raw.len() - 1])),
            _ => (raw, None),
        };
        let steps: Vec<String> = path.split('/').map(str::to_owned).collect();
        validate_select_path(ctx, scope, &steps)?;
        match options {
            Some(text) => select_options(ctx, scope, &steps, text, &mut items, nested)?,
            None => items.push(SelectItem::Path(steps)),
        }
    }
    Ok(items)
}

fn validate_select_path(
    ctx: &OptionContext,
    scope: &Scope,
    steps: &[String],
) -> Result<(), UriError> {
    let unknown = |s: &str| UriError::bad_request(format!("unknown property {s} in $select"));
    let mut current = match scope {
        Scope::Entity(n) => ExprType::Entity(n.clone()),
        Scope::Complex(n) => ExprType::Complex(n.clone()),
        Scope::Applied(a) if a.aggregated => {
            if a.columns.iter().any(|c| c.path.starts_with(steps)) {
                return Ok(());
            }
            return Err(unknown(&steps.join("/")));
        }
        Scope::Applied(a) => {
            if steps.len() == 1 && a.computed.iter().any(|(n, _)| *n == steps[0]) {
                return Ok(());
            }
            ExprType::Entity(a.entity.clone())
        }
        Scope::None | Scope::Value(_) | Scope::CrossJoin(_) => {
            return Err(UriError::bad_request("$select needs a structured resource"));
        }
    };
    for step in steps {
        current = match &current {
            ExprType::Entity(t) => {
                if let Some(p) = ctx.model.property(t, step) {
                    property_type(p)
                } else if ctx.model.navigation(t, step).is_some() {
                    return Ok(());
                } else if ctx.model.entity_type(step).is_some() {
                    ExprType::Entity(ctx.model.local(step).to_owned())
                } else {
                    return Err(unknown(step));
                }
            }
            ExprType::Complex(t) => {
                let c = ctx.model.complex_type(t).ok_or_else(|| unknown(step))?;
                let p = c.properties.iter().find(|p| &p.name == step);
                property_type(p.ok_or_else(|| unknown(step))?)
            }
            _ => return Err(unknown(step)),
        };
    }
    Ok(())
}

fn expand(ctx: &OptionContext, scope: &Scope, value: &str) -> Result<Vec<ExpandItem>, UriError> {
    if ctx.expand_depth >= ctx.limits.max_expand_depth {
        return Err(UriError::bad_request("$expand is nested too deeply"));
    }
    let owner = match scope {
        Scope::Entity(owner) => owner,
        Scope::Applied(a) => &a.entity,
        _ => return Err(UriError::bad_request("$expand needs an entity")),
    };
    let grouped = match scope {
        Scope::Applied(a) if a.aggregated => Some(a),
        _ => None,
    };
    let mut items = Vec::new();
    for raw in split_top_level(value, ',') {
        let raw = raw.trim();
        if grouped.is_some() && raw.starts_with('*') {
            return Err(UriError::bad_request("$expand=* needs entities"));
        }
        if raw == "*" || raw.starts_with("*(") || raw == "*/$ref" {
            for n in ctx.model.all_navigation(owner) {
                let levels = raw.strip_prefix("*(").and_then(|r| r.strip_suffix(')'));
                let mut item = expand_item(ctx, n, Vec::new(), "", raw == "*/$ref", false)?;
                item.from_star = true;
                if let Some(text) = levels {
                    apply_expand_options(ctx, &mut item, text)?;
                }
                items.push(item);
            }
            continue;
        }
        let (path, nested) = match raw.find('(') {
            Some(i) if raw.ends_with(')') => (&raw[..i], &raw[i + 1..raw.len() - 1]),
            _ => (raw, ""),
        };
        let mut steps: Vec<&str> = path.split('/').collect();
        let suffix = match steps.last() {
            Some(&"$ref") | Some(&"$count") => steps.pop(),
            _ => None,
        };
        let (name, prefix) = steps
            .split_last()
            .ok_or_else(|| UriError::bad_request("empty $expand item"))?;
        let mut holder = ExprType::Entity(owner.clone());
        let mut prefix_names = Vec::new();
        for step in prefix {
            holder = match &holder {
                ExprType::Entity(t) if step.contains('.') => {
                    ctx.model.entity_type(step).map_or_else(
                        || Err(UriError::bad_request(format!("unknown type {step}"))),
                        |_| Ok(ExprType::Entity(t.clone())),
                    )?
                }
                ExprType::Entity(t) => match ctx.model.property(t, step).map(property_type) {
                    Some(ty @ ExprType::Complex(_)) => ty,
                    _ => {
                        return Err(UriError::bad_request(format!(
                            "{step} is not a complex property"
                        )));
                    }
                },
                _ => return Err(UriError::bad_request(format!("cannot follow {step}"))),
            };
            prefix_names.push((*step).to_owned());
        }
        let ExprType::Entity(from) = holder else {
            return Err(UriError::not_implemented("navigation inside complex types"));
        };
        let nav = ctx
            .model
            .navigation(&from, name)
            .ok_or_else(|| UriError::bad_request(format!("unknown navigation property {name}")))?;
        // Rows of an aggregation hold the keys they were grouped by, and nothing else to follow.
        if let Some(a) = grouped {
            let has = |k: &String| {
                let path = [nav.name.clone(), k.clone()];
                a.columns.iter().any(|c| c.path == path)
            };
            if nav.collection || !ctx.model.key_of(&nav.target).iter().all(has) {
                return Err(UriError::bad_request(format!(
                    "{name} needs its key among the groupby properties"
                )));
            }
        }
        items.push(expand_item(
            ctx,
            nav,
            prefix_names,
            nested,
            suffix == Some("$ref"),
            suffix == Some("$count"),
        )?);
    }
    Ok(items)
}

fn expand_item(
    ctx: &OptionContext,
    nav: &odatabase_edm::NavigationProperty,
    prefix: Vec<String>,
    nested: &str,
    ref_only: bool,
    count_only: bool,
) -> Result<ExpandItem, UriError> {
    let mut item = ExpandItem {
        prefix,
        navigation: nav.name.clone(),
        target: nav.target.clone(),
        collection: nav.collection,
        options: QueryOptions::default(),
        levels: None,
        count_only,
        ref_only,
        from_star: false,
    };
    apply_expand_options(ctx, &mut item, nested)?;
    Ok(item)
}

pub(crate) fn apply_expand_options(
    ctx: &OptionContext,
    item: &mut ExpandItem,
    nested: &str,
) -> Result<(), UriError> {
    if nested.is_empty() {
        return Ok(());
    }
    let inner = OptionContext {
        model: ctx.model,
        aliases: ctx.aliases,
        limits: ctx.limits,
        expand_depth: ctx.expand_depth + 1,
    };
    let mut scope = Scope::Entity(item.target.clone());
    let parts = split_top_level(nested, ';');
    // `$apply` and `$compute` come first, whatever the order: the other options see their results.
    let find = |name: &str| {
        parts
            .iter()
            .find_map(|p| p.trim().strip_prefix(name)?.strip_prefix('='))
    };
    if let Some(text) = pipeline_text(find("$apply"), find("$compute")) {
        let parsed = parse_apply(&inner, &item.target, &text)?;
        let keeps_rows = parsed.steps.iter().all(|t| {
            matches!(
                t,
                Transformation::Identity
                    | Transformation::Filter(_)
                    | Transformation::Search(_)
                    | Transformation::Compute(_)
            )
        });
        if !keeps_rows && !item.collection {
            return Err(UriError::bad_request(format!(
                "$apply needs a collection, and {} is single-valued",
                item.navigation
            )));
        }
        item.options.apply = parsed.steps;
        item.options.expand.extend(parsed.expand);
        scope = parsed.scope;
    }
    for part in parts {
        let (name, value) = part
            .split_once('=')
            .ok_or_else(|| UriError::bad_request(format!("invalid expand option {part}")))?;
        let name = name.trim();
        if name == "$apply" || name == "$compute" {
            continue;
        }
        if name == "$levels" {
            // `*` reaches every navigation, so only a named one can be the wrong kind.
            let recursive = ctx.model.navigation(&item.target, &item.navigation);
            if recursive.is_none() && !item.from_star {
                return Err(UriError::bad_request(format!(
                    "{} doesn't lead back to its own type",
                    item.navigation
                )));
            }
            item.levels = Some(if value == "max" {
                Levels::Max
            } else {
                Levels::Number(value.parse().map_err(|_| {
                    UriError::bad_request("$levels must be a positive integer or max")
                })?)
            });
            continue;
        }
        if !item.collection && matches!(name, "$top" | "$skip" | "$orderby" | "$count") {
            return Err(UriError::bad_request(format!(
                "{name} is not allowed on single-valued {}",
                item.navigation
            )));
        }
        apply_option(&inner, &scope, &mut item.options, name, value)?;
    }
    if item.options.is_aggregated() && !item.options.expand.is_empty() {
        return Err(UriError::not_implemented(
            "$expand after an aggregation inside $expand",
        ));
    }
    Ok(())
}

/// `$search` grammar: terms and quoted phrases combined by `AND`, `OR`, `NOT`, and adjacency.
pub fn search(text: &str) -> Result<SearchExpr, UriError> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '+' => {
                chars.next();
            }
            '(' | ')' => {
                chars.next();
                tokens.push(c.to_string());
            }
            '"' => {
                chars.next();
                let mut phrase = String::from("\"");
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => phrase.extend(chars.next()),
                        Some(ch) => phrase.push(ch),
                        None => return Err(UriError::bad_request("unterminated $search phrase")),
                    }
                }
                tokens.push(phrase);
            }
            _ => {
                let mut word = String::new();
                while let Some(&ch) = chars.peek() {
                    if matches!(ch, ' ' | '+' | '(' | ')' | '"') {
                        break;
                    }
                    word.push(ch);
                    chars.next();
                }
                tokens.push(word);
            }
        }
    }
    let mut p = SearchParser {
        tokens,
        pos: 0,
        depth: 0,
    };
    let e = p.or()?;
    if p.pos != p.tokens.len() {
        return Err(UriError::bad_request("invalid $search expression"));
    }
    Ok(e)
}

struct SearchParser {
    tokens: Vec<String>,
    pos: usize,
    depth: u32,
}

impl SearchParser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(String::as_str)
    }

    fn or(&mut self) -> Result<SearchExpr, UriError> {
        self.depth += 1;
        if self.depth > 32 {
            return Err(UriError::bad_request("$search is nested too deeply"));
        }
        let mut left = self.and()?;
        while self.peek() == Some("OR") {
            self.pos += 1;
            left = SearchExpr::Or(Box::new(left), Box::new(self.and()?));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn and(&mut self) -> Result<SearchExpr, UriError> {
        let mut left = self.not()?;
        loop {
            match self.peek() {
                Some("AND") => self.pos += 1,
                Some("OR" | ")") | None => return Ok(left),
                _ => {}
            }
            left = SearchExpr::And(Box::new(left), Box::new(self.not()?));
        }
    }

    fn not(&mut self) -> Result<SearchExpr, UriError> {
        match self.peek() {
            Some("NOT") => {
                self.pos += 1;
                self.depth += 1;
                if self.depth > 32 {
                    return Err(UriError::bad_request("$search is nested too deeply"));
                }
                let inner = self.not()?;
                self.depth -= 1;
                Ok(SearchExpr::Not(Box::new(inner)))
            }
            Some("(") => {
                self.pos += 1;
                let e = self.or()?;
                if self.peek() != Some(")") {
                    return Err(UriError::bad_request("expected ) in $search"));
                }
                self.pos += 1;
                Ok(e)
            }
            Some(")") | None => Err(UriError::bad_request("expected a $search term")),
            Some(term) => {
                let term = term.strip_prefix('"').unwrap_or(term).to_owned();
                self.pos += 1;
                Ok(SearchExpr::Term(term))
            }
        }
    }
}
