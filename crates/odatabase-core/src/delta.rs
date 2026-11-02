//! Change tracking: delta links and the responses behind them.
//!
//! A delta page takes the keys of changed entities from the log, then runs the client's query
//! restricted to those keys. Every rule of a normal read applies, and a key the query no longer
//! returns is reported as removed. Types whose rows can be hidden from the caller are not
//! tracked, since such a report would reveal the keys of rows the caller may not see.

use crate::capabilities::stays_on_row;
use crate::format::Format;
use crate::json::Writer;
use crate::paging::{DEFAULT_PAGE_SIZE, PageRequest};
use crate::url::{encode, key_text, next_query, select_list, set_of};
use crate::{ODataError, ODataRequest, ODataResponse, Service};
use http::{HeaderValue, StatusCode};
use odatabase_edm::{Model, TypeKind, Value};
use odatabase_sql::{
    ChangeKind, Page, ReadRequest, Session, change_floor, change_position, read_changes,
    read_entities, record_change,
};
use odatabase_uri::{
    BinaryOp, Expr, ExprType, ODataUri, Path, PathRoot, QueryOptions, Segment, SelectItem, Step,
};
use serde_json::{Map, Value as Json, json};
use std::collections::HashMap;

pub const PREFERENCE_APPLIED: &str = "odata.track-changes";

/// The meaning of `$deltatoken`, which is opaque to clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaToken {
    /// A later page of a first read, which ends in a delta link from `from`.
    FirstRead { from: i64 },
    /// Changes from position `since`. Later pages also know where the following link starts.
    Changes {
        since: i64,
        until: Option<i64>,
        after: i64,
    },
}

impl DeltaToken {
    pub fn parse(text: &str) -> Result<Self, ODataError> {
        let invalid = || ODataError::bad_request("invalid $deltatoken");
        let number = |t: &str| t.parse::<i64>().map_err(|_| invalid());
        let parts: Vec<&str> = text.split('.').collect();
        match parts.as_slice() {
            ["i", from] => Ok(Self::FirstRead {
                from: number(from)?,
            }),
            [since] => Ok(Self::Changes {
                since: number(since)?,
                until: None,
                after: 0,
            }),
            [since, until, after] => Ok(Self::Changes {
                since: number(since)?,
                until: Some(number(until)?),
                after: number(after)?,
            }),
            _ => Err(invalid()),
        }
    }
}

/// Whether the request asks for change tracking.
pub fn prefers_tracking(req: &ODataRequest) -> bool {
    req.headers
        .get_all("prefer")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| matches!(p.trim(), "odata.track-changes" | "track-changes"))
}

/// Whether changes of `type_name` go to the log.
pub fn tracked(model: &Model, type_name: &str) -> bool {
    model.type_chain(type_name).iter().any(|t| t.track_changes)
}

/// Whether a delta link can describe this read: only what the log records can change its result.
pub fn trackable(model: &Model, uri: &ODataUri, type_name: &str) -> bool {
    let o = &uri.options;
    matches!(uri.path.as_slice(), [Segment::EntitySet { .. }])
        && tracked(model, type_name)
        && o.top.is_none()
        && o.skip.is_none()
        && o.expand.is_empty()
        && o.apply.is_empty()
        && o.filter.as_ref().is_none_or(stays_on_row)
}

/// The query of a delta link: the defining query with a new `$deltatoken`.
pub fn delta_query(query: &str, token: &str) -> String {
    let mut parts: Vec<&str> = query
        .split('&')
        .filter(|p| {
            !p.is_empty()
                && !p.starts_with("$skiptoken=")
                && !p.starts_with("%24skiptoken=")
                && !p.starts_with("$deltatoken=")
                && !p.starts_with("%24deltatoken=")
        })
        .collect();
    let token = format!("$deltatoken={}", encode(token));
    parts.push(&token);
    parts.join("&")
}

/// `$filter` matching exactly these keys. The tree stays shallow however many there are.
fn keys_filter(model: &Model, type_name: &str, keys: &[Vec<(String, Value)>]) -> Option<Expr> {
    let property = |name: &str| {
        let ty = match &model.property(type_name, name)?.kind {
            TypeKind::Primitive(k) => ExprType::Primitive(*k),
            TypeKind::Enum(n) => ExprType::Enum(n.clone()),
            _ => return None,
        };
        Some(Expr::Path(Path {
            root: PathRoot::It,
            steps: vec![Step::Property(name.to_owned())],
            ty,
        }))
    };
    let join = |op: BinaryOp, parts: Vec<Expr>| balanced(op, parts);
    let rows = keys
        .iter()
        .map(|key| {
            let parts = key
                .iter()
                .map(|(name, value)| {
                    let equal = Expr::Binary(
                        BinaryOp::Eq,
                        Box::new(property(name)?),
                        Box::new(Expr::Literal(value.clone())),
                    );
                    Some(equal)
                })
                .collect::<Option<Vec<_>>>()?;
            join(BinaryOp::And, parts)
        })
        .collect::<Option<Vec<_>>>()?;
    join(BinaryOp::Or, rows)
}

fn balanced(op: BinaryOp, mut parts: Vec<Expr>) -> Option<Expr> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        n => {
            let right = parts.split_off(n / 2);
            Some(Expr::Binary(
                op,
                Box::new(balanced(op, parts)?),
                Box::new(balanced(op, right)?),
            ))
        }
    }
}

/// The options that read the changed rows of a page.
fn page_options(
    model: &Model,
    type_name: &str,
    o: &QueryOptions,
    keys: &[Vec<(String, Value)>],
) -> Result<QueryOptions, ODataError> {
    let mut options = o.clone();
    let only = keys_filter(model, type_name, keys)
        .ok_or_else(|| ODataError::new(500, "a key cannot be filtered on"))?;
    options.filter = Some(match options.filter.take() {
        Some(f) => Expr::Binary(BinaryOp::And, Box::new(f), Box::new(only)),
        None => only,
    });
    options.orderby.clear();
    options.count = false;
    options.skiptoken = None;
    options.deltatoken = None;
    // A row says which entity it is, whatever `$select` asked for.
    if let Some(select) = &mut options.select
        && !select.contains(&SelectItem::Star)
    {
        for k in model.key_of(type_name) {
            let path = SelectItem::Path(vec![k.clone()]);
            if !select.contains(&path) {
                select.push(path);
            }
        }
    }
    Ok(options)
}

impl Service {
    /// Notes a change in the transaction of `session`, if its type is tracked.
    pub(crate) async fn record(
        &self,
        session: &Session,
        type_name: &str,
        key: &[(String, Value)],
        kind: ChangeKind,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        if !tracked(model, type_name) {
            return Ok(());
        }
        let set = set_of(model, type_name)
            .ok_or_else(|| ODataError::new(500, format!("{type_name} has no entity set")))?;
        record_change(session, set, &key_text(key), kind).await?;
        Ok(())
    }

    /// An entry that takes an entity out of a client's result.
    pub(crate) fn removed_entry(
        &self,
        req: &ODataRequest,
        set: &str,
        key: &str,
        reason: &str,
    ) -> Json {
        json!({
            "@odata.context": format!("{}/$metadata#{set}/$deletedEntity", req.root),
            "id": format!("{}/{set}{key}", req.root),
            "reason": reason,
        })
    }

    /// One page of changes.
    pub(crate) async fn delta_page(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        type_name: &str,
        session: &Session,
        format: Format,
        (since, until, after): (i64, Option<i64>, i64),
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let o = &uri.options;
        let set = set_of(model, type_name)
            .ok_or_else(|| ODataError::new(500, format!("{type_name} has no entity set")))?;
        if since < change_floor(session).await? {
            return Err(ODataError::new(410, "the delta link has expired"));
        }
        // Read first, so that a change racing this page is in the next one.
        let until = match until {
            Some(until) => until,
            None => change_position(session).await?,
        };
        let page = self.inner.paging.page(&PageRequest {
            entity_type: type_name,
            top: None,
            skip: None,
            skiptoken: None,
            max_page_size: crate::read::max_page_size(req),
            claims: &req.claims,
        })?;
        let size = page.top.unwrap_or(DEFAULT_PAGE_SIZE);
        let mut changes = read_changes(session, set, since, after, size + 1).await?;
        let more = changes.len() as u64 > size;
        changes.truncate(size as usize);

        // A key changed twice on this page appears once, with its latest kind.
        let mut order: Vec<String> = Vec::new();
        let mut keys: HashMap<String, Vec<(String, Value)>> = HashMap::new();
        let mut kinds: HashMap<String, ChangeKind> = HashMap::new();
        for change in &changes {
            let Some(key) = self.logged_key(set, &change.key) else {
                tracing::warn!(key = %change.key, set, "skipping an unreadable change log row");
                continue;
            };
            let text = key_text(&key);
            if keys.insert(text.clone(), key).is_none() {
                order.push(text.clone());
            }
            kinds.insert(text, change.kind);
        }
        let key_list: Vec<_> = order.iter().filter_map(|k| keys.get(k).cloned()).collect();
        // The query runs even without keys: it is what checks the caller may see this data.
        let options = match key_list.is_empty() {
            true => o.clone(),
            false => page_options(model, type_name, o, &key_list)?,
        };
        let window = Page {
            top: key_list.is_empty().then_some(0),
            skip: None,
        };
        let language = crate::read::language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &language, session);
        let read = ReadRequest {
            segments: &uri.path,
            type_name,
            options: &options,
            page: window,
            limits: self.inner.config.expand_limits,
        };
        let mut records = read_entities(&ctx, session, read).await?;
        crate::transient::apply(&self.inner, &mut records);
        let w = Writer {
            model,
            root: &req.root,
            format,
        };
        let mut found: HashMap<String, Json> = HashMap::new();
        for rec in &records {
            let key: Vec<(String, Value)> = model
                .key_of(type_name)
                .iter()
                .filter_map(|k| Some((k.clone(), rec.value(k)?.clone())))
                .collect();
            found.insert(key_text(&key), Json::Object(w.entity(rec, type_name)));
        }
        let items: Vec<Json> = order
            .iter()
            .map(|key| match found.remove(key) {
                Some(row) => row,
                None => {
                    let reason = match kinds.get(key) {
                        Some(ChangeKind::Deleted) => "deleted",
                        _ => "changed",
                    };
                    self.removed_entry(req, set, key, reason)
                }
            })
            .collect();

        let mut body = Map::new();
        if format.metadata != crate::MetadataLevel::None {
            let context = format!("{}/$metadata#{set}{}/$delta", req.root, select_list(o));
            body.insert("@odata.context".into(), context.into());
        }
        body.insert("value".into(), items.into());
        let url = format!("{}/{}", req.root, req.path.trim_matches('/'));
        match changes.last() {
            Some(last) if more => {
                let token = format!("{since}.{until}.{}", last.id);
                let link = format!("{url}?{}", delta_query(&req.query, &token));
                body.insert("@odata.nextLink".into(), link.into());
            }
            _ => {
                let link = format!("{url}?{}", delta_query(&req.query, &until.to_string()));
                body.insert("@odata.deltaLink".into(), link.into());
            }
        }
        Ok(ODataResponse::json(
            StatusCode::OK,
            format.content_type(),
            &Json::Object(body),
        ))
    }

    /// The key of a log row, which a trigger may have written in any valid form.
    fn logged_key(&self, set: &str, text: &str) -> Option<Vec<(String, Value)>> {
        let model = &self.inner.model;
        let uri = odatabase_uri::parse(model, &format!("{set}{text}"), "").ok()?;
        let Some(Segment::Key { values, target }) = uri.path.last() else {
            return None;
        };
        let odatabase_uri::Target::Entity(t) = target else {
            return None;
        };
        model
            .key_of(t)
            .iter()
            .map(|k| Some((k.clone(), values.iter().find(|(n, _)| n == k)?.1.clone())))
            .collect()
    }

    /// Adds the end of a tracked read to its body and headers.
    pub(crate) fn finish_tracked(
        req: &ODataRequest,
        from: i64,
        last_page: bool,
        body: &mut Map<String, Json>,
    ) {
        if last_page {
            let url = format!("{}/{}", req.root, req.path.trim_matches('/'));
            let link = format!("{url}?{}", delta_query(&req.query, &from.to_string()));
            body.insert("@odata.deltaLink".into(), link.into());
        }
    }

    /// Marks a response as tracked.
    pub(crate) fn tracked_response(r: &mut ODataResponse) {
        r.headers.append(
            "preference-applied",
            HeaderValue::from_static(PREFERENCE_APPLIED),
        );
    }

    /// The token that keeps later pages of a tracked read on the same delta link.
    pub(crate) fn first_read_link(query: &str, from: i64, next: &str) -> String {
        let keep = delta_query(query, &format!("i.{from}"));
        next_query(&keep, next)
    }
}
