//! `$search` over the properties marked `searchable`.

use crate::filter::{Frame, Translator, tpl};
use crate::{Dialect, QueryError};
use odatabase_edm::{Property, TypeKind};
use odatabase_uri::SearchExpr;
use sea_query::{Expr, ExprTrait};

/// A condition matching `search` in any searchable property of `type_name`.
///
/// Postgres uses text search on the concatenated properties. SQLite has no built-in text
/// search without a dedicated FTS table, so it falls back to substring matching.
pub fn search_condition(
    tr: &mut Translator<'_>,
    frame: &Frame,
    type_name: &str,
    search: &SearchExpr,
) -> Result<Expr, QueryError> {
    let props: Vec<&Property> = tr
        .ctx
        .model
        .all_properties(type_name)
        .into_iter()
        .filter(|p| p.searchable && matches!(p.kind, TypeKind::Primitive(_)))
        .collect();
    if props.is_empty() {
        return Err(QueryError::bad_request(format!(
            "{type_name} is not searchable"
        )));
    }
    let mut columns = Vec::with_capacity(props.len());
    for p in &props {
        columns.push(tr.scalar(frame, p)?);
    }
    term(tr.ctx.dialect, &columns, search)
}

fn term(dialect: Dialect, columns: &[Expr], search: &SearchExpr) -> Result<Expr, QueryError> {
    Ok(match search {
        SearchExpr::And(l, r) => term(dialect, columns, l)?.and(term(dialect, columns, r)?),
        SearchExpr::Or(l, r) => term(dialect, columns, l)?.or(term(dialect, columns, r)?),
        SearchExpr::Not(inner) => term(dialect, columns, inner)?.not(),
        SearchExpr::Term(text) => {
            let mut any: Option<Expr> = None;
            for c in columns {
                let e = match dialect {
                    Dialect::Postgres => tpl(
                        dialect,
                        "(to_tsvector('simple', coalesce(CAST($1 AS text), '')) @@ phraseto_tsquery('simple', $2))",
                        [c.clone(), Expr::val(text.clone())],
                    ),
                    Dialect::Sqlite => tpl(
                        dialect,
                        "(instr(lower($1), lower($2)) > 0)",
                        [c.clone(), Expr::val(text.clone())],
                    ),
                };
                any = Some(match any {
                    Some(prev) => prev.or(e),
                    None => e,
                });
            }
            any.ok_or_else(|| QueryError::internal("no searchable columns"))?
        }
    })
}
