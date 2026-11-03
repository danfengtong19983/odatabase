//! `$all` and `$crossjoin`: reads that aren't about one entity set.

use crate::format::Format;
use crate::json::Writer;
use crate::paging::PageRequest;
use crate::read::{language, max_page_size};
use crate::url::{encode, key_text, next_query};
use crate::{ODataError, ODataRequest, ODataResponse, Service};
use http::{Method, StatusCode};
use odatabase_edm::Value;
use odatabase_sql::{
    Page, ReadRequest, Session, count_collection, count_crossjoin, read_crossjoin, read_entities,
};
use odatabase_uri::{ODataUri, Target};
use serde_json::{Map, Value as Json};
use std::collections::HashMap;

impl Service {
    /// Answers a request for `$all` or `$crossjoin`, if this is one.
    pub(crate) async fn whole_service(
        &self,
        req: &ODataRequest,
    ) -> Option<Result<ODataResponse, ODataError>> {
        let path = req.path.trim_matches('/');
        let path = match path.strip_prefix("%24") {
            Some(rest) => format!("${rest}"),
            None => path.to_owned(),
        };
        let cast = match path.strip_prefix("$all") {
            Some("") => Some(None),
            Some(rest) => rest.strip_prefix('/').map(Some),
            None => None,
        };
        let sets = path
            .strip_prefix("$crossjoin(")
            .and_then(|p| p.strip_suffix(')'));
        if cast.is_none() && sets.is_none() {
            return None;
        }
        if req.method != Method::GET {
            return Some(Err(ODataError::new(
                405,
                format!("{path} can only be read"),
            )));
        }
        Some(match (cast, sets) {
            (Some(cast), _) => self.all(req, cast).await,
            (_, Some(sets)) => self.cross_join(req, sets).await,
            _ => Err(ODataError::not_found("no such resource")),
        })
    }

    /// The sets that can answer the query, each parsed for itself. A set the query doesn't fit
    /// or the caller may not read adds nothing, rather than failing the others.
    async fn all_sources(
        &self,
        req: &ODataRequest,
        session: &Session,
        cast: Option<&str>,
    ) -> Result<Vec<(ODataUri, String, i64)>, ODataError> {
        let model = &self.inner.model;
        let mut names: Vec<&String> = model.entity_sets.keys().collect();
        names.sort();
        let wanted = cast.map(|c| model.local(c).to_owned());
        if let Some(t) = &wanted
            && model.entity_type(t).is_none()
        {
            return Err(ODataError::not_found(format!("unknown type {t}")));
        }
        let (mut sources, mut refused) = (Vec::new(), None);
        for name in names {
            let set = &model.entity_sets[name];
            let path = match (&wanted, cast) {
                (Some(t), Some(cast)) => {
                    if !model
                        .type_chain(t)
                        .iter()
                        .any(|b| b.name == set.entity_type)
                    {
                        continue;
                    }
                    format!("{name}/{cast}")
                }
                _ => name.clone(),
            };
            match self.all_source(req, session, &path).await {
                Ok(source) => sources.push(source),
                Err(e) => refused = refused.or(Some(e)),
            }
        }
        match (sources.is_empty(), refused) {
            (true, Some(e)) => Err(e),
            _ => Ok(sources),
        }
    }

    async fn all_source(
        &self,
        req: &ODataRequest,
        session: &Session,
        path: &str,
    ) -> Result<(ODataUri, String, i64), ODataError> {
        let model = &self.inner.model;
        let limits = self.inner.config.uri_limits;
        let uri = odatabase_uri::parse_with(model, path, &req.query, limits)?;
        if !uri.options.apply.is_empty() {
            return Err(ODataError::bad_request("$apply needs an entity set"));
        }
        let Target::EntityCollection(t) = uri.target().clone() else {
            return Err(ODataError::bad_request(format!("{path} is not a set")));
        };
        self.check_selected_visibility(req, &uri)?;
        crate::capabilities::check_read(model, &t, &uri)?;
        let lang = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &lang, session);
        let n = count_collection(&ctx, session, &uri.path, &t, &uri.options).await?;
        Ok((uri, t, n))
    }

    async fn all(
        &self,
        req: &ODataRequest,
        cast: Option<&str>,
    ) -> Result<ODataResponse, ODataError> {
        let session = Session::from(self.inner.database.clone());
        let sources = self.all_sources(req, &session, cast).await?;
        let Some((first, ..)) = sources.first() else {
            return Err(ODataError::not_found("no entity set matches"));
        };
        let o = &first.options;
        let format = Format::negotiate(o.format.as_deref(), &req.headers)?;
        let page = self.inner.paging.page(&PageRequest {
            entity_type: "",
            top: o.top,
            skip: o.skip,
            skiptoken: o.skiptoken.as_deref(),
            max_page_size: max_page_size(req),
            claims: &req.claims,
        })?;
        let mut left = match (page.top, page.next.is_some()) {
            (Some(top), true) => Some(top + 1),
            (top, _) => top,
        };
        let mut skip = page.skip.unwrap_or(0) as i64;
        let lang = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &lang, &session);
        let mut records = Vec::new();
        for (uri, t, n) in &sources {
            if left == Some(0) {
                break;
            }
            if skip >= *n {
                skip -= n;
                continue;
            }
            let mut options = uri.options.clone();
            options.top = None;
            options.skip = None;
            options.count = false;
            options.skiptoken = None;
            let read = ReadRequest {
                segments: &uri.path,
                type_name: t,
                options: &options,
                page: Page {
                    top: left,
                    skip: (skip > 0).then_some(skip as u64),
                },
                limits: self.inner.config.expand_limits,
            };
            let rows = read_entities(&ctx, &session, read).await?;
            skip = 0;
            left = left.map(|l| l.saturating_sub(rows.len() as u64));
            records.extend(rows);
        }
        crate::transient::apply(&self.inner, &mut records);
        let more = page.next.is_some() && page.top.is_some_and(|t| records.len() as u64 > t);
        if more && let Some(top) = page.top {
            records.truncate(top as usize);
        }
        let w = Writer {
            model: &self.inner.model,
            root: &req.root,
            format,
        };
        let none = format.metadata == crate::MetadataLevel::None;
        let items: Vec<Json> = records
            .iter()
            .map(|r| {
                let mut row = Map::new();
                // The context names no set, so each row says where it lives.
                if !none && let Some(id) = crate::url::entity_id(&self.inner.model, &r.type_name, r)
                {
                    row.insert("@odata.id".into(), format!("{}/{id}", req.root).into());
                }
                row.extend(w.entity(r, ""));
                Json::Object(row)
            })
            .collect();
        let mut body = Map::new();
        if !none {
            let context = format!("{}/$metadata#Collection(Edm.EntityType)", req.root);
            body.insert("@odata.context".into(), context.into());
            if o.count {
                let total: i64 = sources.iter().map(|(.., n)| *n).sum();
                body.insert("@odata.count".into(), total.into());
            }
        }
        body.insert("value".into(), items.into());
        if more && let Some(next) = &page.next {
            let link = format!(
                "{}/{}?{}",
                req.root,
                req.path.trim_matches('/'),
                next_query(&req.query, next)
            );
            body.insert("@odata.nextLink".into(), link.into());
        }
        Ok(ODataResponse::json(
            StatusCode::OK,
            format.content_type(),
            &Json::Object(body),
        ))
    }

    async fn cross_join(
        &self,
        req: &ODataRequest,
        names: &str,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let limits = self.inner.config.uri_limits;
        let (sets, o) = odatabase_uri::parse_crossjoin(model, names, &req.query, limits)?;
        let format = Format::negotiate(o.format.as_deref(), &req.headers)?;
        let session = Session::from(self.inner.database.clone());
        let lang = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &lang, &session);
        let page = self.inner.paging.page(&PageRequest {
            entity_type: "",
            top: o.top,
            skip: o.skip,
            skiptoken: o.skiptoken.as_deref(),
            max_page_size: max_page_size(req),
            claims: &req.claims,
        })?;
        let window = Page {
            top: match (page.top, page.next.is_some()) {
                (Some(top), true) => Some(top + 1),
                (top, _) => top,
            },
            skip: page.skip,
        };
        let mut rows = read_crossjoin(&ctx, &session, &sets, &o, window).await?;
        let more = page.next.is_some() && page.top.is_some_and(|t| rows.len() as u64 > t);
        if more && let Some(top) = page.top {
            rows.truncate(top as usize);
        }
        let w = Writer {
            model,
            root: &req.root,
            format,
        };
        let mut expanded: HashMap<(usize, String), Json> = HashMap::new();
        for item in &o.expand {
            let Some(i) = sets.iter().position(|(s, _)| *s == item.navigation) else {
                continue;
            };
            let (set, type_name) = &sets[i];
            let mut keys: Vec<Vec<(String, Value)>> = rows.iter().map(|r| r[i].clone()).collect();
            keys.sort_by_key(|k| key_text(k));
            keys.dedup();
            let found = self
                .entities_with_keys(req, &session, set, type_name, &item.options, &keys)
                .await?;
            for rec in &found {
                let key: Vec<(String, Value)> = model
                    .key_of(type_name)
                    .iter()
                    .filter_map(|k| Some((k.clone(), rec.value(k)?.clone())))
                    .collect();
                expanded.insert((i, key_text(&key)), Json::Object(w.entity(rec, type_name)));
            }
        }
        let items: Vec<Json> = rows
            .iter()
            .map(|row| {
                let mut out = Map::new();
                for (i, ((set, _), key)) in sets.iter().zip(row).enumerate() {
                    let text = key_text(key);
                    out.insert(
                        format!("{set}@odata.navigationLink"),
                        format!("{set}{text}").into(),
                    );
                    if let Some(entity) = expanded.get(&(i, text)) {
                        out.insert(set.clone(), entity.clone());
                    }
                }
                Json::Object(out)
            })
            .collect();
        let mut body = Map::new();
        if format.metadata != crate::MetadataLevel::None {
            let context = format!("{}/$metadata#Collection(Edm.ComplexType)", req.root);
            body.insert("@odata.context".into(), context.into());
            if o.count {
                let n = count_crossjoin(&ctx, &session, &sets, &o).await?;
                body.insert("@odata.count".into(), n.into());
            }
        }
        body.insert("value".into(), items.into());
        if more && let Some(next) = &page.next {
            let link = format!(
                "{}/$crossjoin({})?{}",
                req.root,
                encode(names),
                next_query(&req.query, next)
            );
            body.insert("@odata.nextLink".into(), link.into());
        }
        Ok(ODataResponse::json(
            StatusCode::OK,
            format.content_type(),
            &Json::Object(body),
        ))
    }
}
