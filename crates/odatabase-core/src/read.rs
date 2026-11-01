//! `GET` requests.

use crate::format::{Format, metadata_as_json};
use crate::json::{Writer, etag_header};
use crate::paging::PageRequest;
use crate::url::{aggregated_list, computed_list, entity_id, next_query, select_list, set_path};
use crate::{ODataError, ODataRequest, ODataResponse, Service};
use http::{HeaderValue, StatusCode, header};
use odatabase_edm::{TypeKind, to_csdl_json, to_csdl_xml};
use odatabase_sql::{
    Node, Page, QueryContext, ReadRequest, Record, Session, count_collection, read_aggregated,
    read_collection, read_columns, read_entities,
};
use odatabase_uri::{ODataUri, QueryOptions, Segment, SelectItem, Target};
use serde_json::{Map, Value as Json, json};

/// The value of `Prefer: odata.maxpagesize`.
fn max_page_size(req: &ODataRequest) -> Option<u64> {
    req.headers
        .get_all("prefer")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|p| {
            let (name, value) = p.split_once('=')?;
            matches!(name.trim(), "odata.maxpagesize" | "maxpagesize")
                .then(|| value.trim().parse().ok())?
        })
}

/// The first `Accept-Language` entry, reduced to its primary subtag.
pub(crate) fn language(req: &ODataRequest, default: &str) -> String {
    req.headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| {
            v.split([';', '-'])
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|l| !l.is_empty() && l != "*")
        .unwrap_or_else(|| default.to_owned())
}

/// The longest path prefix that addresses entities, and the properties after it.
pub(crate) fn split_entity_path(path: &[Segment]) -> (&[Segment], &[Segment]) {
    let last = path
        .iter()
        .rposition(|s| matches!(s.target(), Target::Entity(_) | Target::EntityCollection(_)));
    match last {
        Some(i) => path.split_at(i + 1),
        None => match path
            .iter()
            .position(|s| matches!(s, Segment::Function { .. }))
        {
            Some(i) if i + 1 < path.len() => path.split_at(i + 1),
            _ => (&[], path),
        },
    }
}

impl Service {
    pub(crate) fn query_context<'a>(
        &'a self,
        req: &'a ODataRequest,
        language: &'a str,
        session: &Session,
    ) -> QueryContext<'a> {
        QueryContext {
            model: &self.inner.model,
            dialect: session.dialect(),
            claims: &req.claims,
            language,
            extensions: &self.inner.extensions,
        }
    }

    pub(crate) async fn get(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
    ) -> Result<ODataResponse, ODataError> {
        let session = Session::from(self.inner.database.clone());
        self.get_in(req, uri, &session).await
    }

    /// Answers a `GET` on `session`, which is a transaction inside a change set.
    pub(crate) async fn get_in(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        self.check_selected_visibility(req, uri)?;
        if let Some(t) = capability_type(uri) {
            crate::capabilities::check_read(&self.inner.model, t, uri)?;
        }
        if let Some(Segment::Function { .. }) = uri.path.last()
            && !self.composable_result(uri)
        {
            return self.call_function(req, uri, session).await;
        }
        match uri.target() {
            Target::Metadata => self.metadata(req, uri),
            Target::Root => self.service_document(req, uri),
            Target::Batch => Err(ODataError::new(405, "$batch needs POST")),
            Target::EntityCollection(t) => self.collection(req, uri, t, session).await,
            Target::Entity(t) => self.single(req, uri, t, session).await,
            Target::Count => self.count(req, uri, session).await,
            Target::Primitive(_)
            | Target::PrimitiveCollection(_)
            | Target::Complex(_)
            | Target::ComplexCollection(_) => self.property(req, uri, session).await,
            Target::RawValue => self.raw_value(req, uri, session).await,
            Target::Ref { .. } => self.references(req, uri, session).await,
            Target::ActionResult | Target::Void => {
                Err(ODataError::new(405, "this resource cannot be read"))
            }
        }
    }

    /// Every property along a path must be visible to the caller.
    pub(crate) fn check_path_visible(
        &self,
        req: &ODataRequest,
        type_name: &str,
        names: &[&str],
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let hidden = |name: &str| ODataError::new(403, format!("{name} is not accessible"));
        let Some(first) = names.first() else {
            return Ok(());
        };
        let mut current = model.property(type_name, first);
        for (i, name) in names.iter().enumerate() {
            let Some(p) = current else {
                return Ok(());
            };
            if !req.claims.sees(&p.visible_for) {
                return Err(hidden(name));
            }
            current = match (&p.kind, names.get(i + 1)) {
                (TypeKind::Complex(t), Some(next)) => model
                    .complex_type(t)
                    .and_then(|c| c.properties.iter().find(|q| q.name == **next)),
                _ => None,
            };
        }
        Ok(())
    }

    /// A property named in `$select` must be visible, even though `*` silently skips it.
    fn check_selected_visibility(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
    ) -> Result<(), ODataError> {
        let (Some(items), Some(t)) = (&uri.options.select, target_type(uri.target())) else {
            return Ok(());
        };
        for item in items {
            let SelectItem::Path(path) = item else {
                continue;
            };
            let hidden = path.first().and_then(|n| self.inner.model.property(t, n));
            if hidden.is_some_and(|p| !req.claims.sees(&p.visible_for)) {
                return Err(ODataError::new(
                    403,
                    format!("{} is not accessible", path[0]),
                ));
            }
        }
        Ok(())
    }

    fn metadata(&self, req: &ODataRequest, uri: &ODataUri) -> Result<ODataResponse, ODataError> {
        let filtered = crate::visibility::visible_model(&self.inner.model, &req.claims);
        let model = filtered.as_ref().unwrap_or(&self.inner.model);
        let mut response = if metadata_as_json(uri.options.format.as_deref(), &req.headers)? {
            let body = to_csdl_json(model);
            ODataResponse::json(
                StatusCode::OK,
                HeaderValue::from_static("application/json"),
                &body,
            )
        } else {
            ODataResponse::with_body(StatusCode::OK, "application/xml", to_csdl_xml(model))
        };
        let tag = format!("\"{:016x}\"", fingerprint(&response.body));
        if let Some(v) = req
            .headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            && etag_matches(v, &tag)
        {
            response = ODataResponse::new(StatusCode::NOT_MODIFIED);
        }
        if let Ok(value) = HeaderValue::try_from(tag) {
            response.headers.insert(header::ETAG, value);
        }
        Ok(response)
    }

    fn service_document(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
    ) -> Result<ODataResponse, ODataError> {
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let filtered = crate::visibility::visible_model(&self.inner.model, &req.claims);
        let m = filtered.as_ref().unwrap_or(&self.inner.model);
        let mut value = Vec::new();
        for s in m
            .entity_sets
            .values()
            .filter(|s| s.include_in_service_document)
        {
            value.push(json!({"name": s.name, "kind": "EntitySet", "url": s.name}));
        }
        for s in m.singletons.values() {
            value.push(json!({"name": s.name, "kind": "Singleton", "url": s.name}));
        }
        for (name, ops) in &m.functions {
            if ops.iter().any(|o| !o.is_bound && o.parameters.is_empty()) {
                value.push(json!({"name": name, "kind": "FunctionImport", "url": name}));
            }
        }
        let body = json!({"@odata.context": format!("{}/$metadata", req.root), "value": value});
        Ok(ODataResponse::json(
            StatusCode::OK,
            format.content_type(),
            &body,
        ))
    }

    fn limits(&self) -> odatabase_sql::ExpandLimits {
        self.inner.config.expand_limits
    }

    async fn collection(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        type_name: &str,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let o = &uri.options;
        let format = Format::negotiate(o.format.as_deref(), &req.headers)?;
        let language = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &language, session);
        let page = self.inner.paging.page(&PageRequest {
            entity_type: type_name,
            top: o.top,
            skip: o.skip,
            skiptoken: o.skiptoken.as_deref(),
            max_page_size: max_page_size(req),
            claims: &req.claims,
        })?;
        // One extra row tells whether a next page exists.
        let fetch = match (page.top, page.next.is_some()) {
            (Some(top), true) => Some(top + 1),
            (top, _) => top,
        };
        let window = Page {
            top: fetch,
            skip: page.skip,
        };
        let mut records = if o.is_aggregated() {
            read_aggregated(&ctx, session, &uri.path, type_name, o, window).await?
        } else {
            let read = ReadRequest {
                segments: &uri.path,
                type_name,
                options: o,
                page: window,
                limits: self.limits(),
            };
            read_entities(&ctx, session, read).await?
        };
        crate::transient::apply(&self.inner, &mut records);
        let has_more =
            page.next.is_some() && page.top.is_some_and(|top| records.len() as u64 > top);
        if has_more && let Some(top) = page.top {
            records.truncate(top as usize);
        }
        let w = self.writer(req, format);
        let mut body = Map::new();
        if format.metadata != crate::MetadataLevel::None {
            body.insert(
                "@odata.context".into(),
                self.context_collection(req, type_name, o).into(),
            );
        }
        if o.count && format.metadata != crate::MetadataLevel::None {
            let n = count_collection(&ctx, session, &uri.path, type_name, o).await?;
            body.insert("@odata.count".into(), n.into());
        }
        let items: Vec<Json> = records
            .iter()
            .map(|r| match o.is_aggregated() {
                true => Json::Object(w.aggregated(r, type_name)),
                false => Json::Object(w.entity(r, type_name)),
            })
            .collect();
        body.insert("value".into(), items.into());
        if has_more && let Some(next) = &page.next {
            let link = format!(
                "{}/{}?{}",
                req.root,
                req.path.trim_matches('/'),
                next_query(&req.query, next)
            );
            body.insert("@odata.nextLink".into(), link.into());
        }
        let mut r = ODataResponse::json(StatusCode::OK, format.content_type(), &Json::Object(body));
        if page.applied_max_page_size
            && let Some(n) = max_page_size(req)
            && let Ok(v) = HeaderValue::try_from(format!("odata.maxpagesize={n}"))
        {
            r.headers.insert("preference-applied", v);
        }
        Ok(r)
    }

    fn writer<'a>(&'a self, req: &'a ODataRequest, format: Format) -> Writer<'a> {
        Writer {
            model: &self.inner.model,
            root: &req.root,
            format,
        }
    }

    fn context_collection(&self, req: &ODataRequest, t: &str, o: &QueryOptions) -> String {
        let m = &self.inner.model;
        match set_path(m, t) {
            Some(set) if o.is_aggregated() => {
                format!("{}/$metadata#{set}{}", req.root, aggregated_list(o))
            }
            Some(set) => {
                let list = computed_list(o).unwrap_or_else(|| select_list(o));
                format!("{}/$metadata#{set}{list}", req.root)
            }
            None => format!("{}/$metadata#Collection({})", req.root, m.qualified(t)),
        }
    }

    fn context_entity(&self, req: &ODataRequest, t: &str, o: &QueryOptions) -> String {
        let m = &self.inner.model;
        if let Some(s) = m.singletons.values().find(|s| s.entity_type == t) {
            return format!("{}/$metadata#{}{}", req.root, s.name, select_list(o));
        }
        match set_path(m, t) {
            Some(set) => format!("{}/$metadata#{set}{}/$entity", req.root, select_list(o)),
            None => format!("{}/$metadata#{}", req.root, m.qualified(t)),
        }
    }

    pub(crate) async fn read_one(
        &self,
        req: &ODataRequest,
        session: &Session,
        segments: &[Segment],
        type_name: &str,
        options: &QueryOptions,
    ) -> Result<Option<Record>, ODataError> {
        let language = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &language, session);
        let records = read_entities(
            &ctx,
            session,
            ReadRequest {
                segments,
                type_name,
                options,
                page: Page {
                    top: Some(1),
                    skip: None,
                },
                limits: self.limits(),
            },
        )
        .await?;
        let mut records = records;
        crate::transient::apply(&self.inner, &mut records);
        Ok(records.into_iter().next())
    }

    /// Serializes a single entity with its context, for reads and write responses.
    pub(crate) fn entity_response(
        &self,
        req: &ODataRequest,
        rec: &Record,
        type_name: &str,
        options: &QueryOptions,
        format: Format,
        status: StatusCode,
    ) -> ODataResponse {
        let w = self.writer(req, format);
        let mut body = Map::new();
        if format.metadata != crate::MetadataLevel::None {
            body.insert(
                "@odata.context".into(),
                self.context_entity(req, type_name, options).into(),
            );
        }
        body.extend(w.entity(rec, type_name));
        let mut r = ODataResponse::json(status, format.content_type(), &Json::Object(body));
        if let Some(etag) = w.etag(rec).and_then(|e| HeaderValue::try_from(e).ok()) {
            r.headers.insert(header::ETAG, etag);
        }
        r
    }

    async fn single(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        type_name: &str,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let Some(rec) = self
            .read_one(req, session, &uri.path, type_name, &uri.options)
            .await?
        else {
            if matches!(uri.path.last(), Some(Segment::Navigation { .. })) {
                return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
            }
            return Err(ODataError::not_found("the entity does not exist"));
        };
        if let Some(not_modified) = self.check_read_preconditions(req, &rec)? {
            return Ok(not_modified);
        }
        Ok(self.entity_response(req, &rec, type_name, &uri.options, format, StatusCode::OK))
    }

    /// Applies `If-None-Match` and `If-Match` to a read.
    fn check_read_preconditions(
        &self,
        req: &ODataRequest,
        rec: &Record,
    ) -> Result<Option<ODataResponse>, ODataError> {
        let m = &self.inner.model;
        let Some(p) = m.etag_property(&rec.type_name) else {
            return Ok(None);
        };
        let Some(current) = rec.value(&p.name).map(etag_header) else {
            return Ok(None);
        };
        if let Some(v) = req
            .headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            && etag_matches(v, &current)
        {
            let mut r = ODataResponse::new(StatusCode::NOT_MODIFIED);
            if let Ok(tag) = HeaderValue::try_from(current) {
                r.headers.insert(header::ETAG, tag);
            }
            return Ok(Some(r));
        }
        if let Some(v) = req
            .headers
            .get(header::IF_MATCH)
            .and_then(|v| v.to_str().ok())
            && !etag_matches(v, &current)
        {
            return Err(ODataError::new(412, "the entity tag does not match"));
        }
        Ok(None)
    }

    async fn count(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let language = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &language, session);
        let body_path = &uri.path[..uri.path.len() - 1];
        let (entity_path, tail) = split_entity_path(body_path);
        let n = if let [Segment::Property { name, .. }] = tail {
            if let Some(Target::Entity(t)) = entity_path.last().map(Segment::target) {
                self.check_path_visible(req, t, &[name.as_str()])?;
            }
            let Some((rec, t)) = self.owner(req, session, entity_path, name).await? else {
                return Err(ODataError::not_found("the entity does not exist"));
            };
            read_collection(&ctx, session, &rec, &t, name).await?.len() as i64
        } else {
            let t = match body_path.last().map(Segment::target) {
                Some(Target::EntityCollection(t)) => t,
                _ => return Err(ODataError::not_implemented("this $count")),
            };
            count_collection(&ctx, session, body_path, t, &uri.options).await?
        };
        Ok(ODataResponse::with_body(
            StatusCode::OK,
            "text/plain",
            n.to_string(),
        ))
    }

    /// Reads the entity owning a property, with only that property selected.
    async fn owner(
        &self,
        req: &ODataRequest,
        session: &Session,
        path: &[Segment],
        property: &str,
    ) -> Result<Option<(Record, String)>, ODataError> {
        let t = match path.last().map(Segment::target) {
            Some(Target::Entity(t)) => t.clone(),
            _ => return Err(ODataError::not_found("the entity does not exist")),
        };
        let options = QueryOptions {
            select: Some(vec![SelectItem::Path(vec![property.to_owned()])]),
            ..QueryOptions::default()
        };
        Ok(self
            .read_one(req, session, path, &t, &options)
            .await?
            .map(|r| (r, t)))
    }

    /// The value at a property path, or `None` when the owning entity does not exist.
    async fn read_property(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<Option<(Node, String, Record)>, ODataError> {
        let path = match uri.path.last() {
            Some(Segment::Value | Segment::Ref) => &uri.path[..uri.path.len() - 1],
            _ => &uri.path[..],
        };
        let (entity_path, tail) = split_entity_path(path);
        let names: Vec<&str> = tail
            .iter()
            .filter_map(|s| match s {
                Segment::Property { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let Some(first) = names.first() else {
            return Err(ODataError::not_implemented("this property path"));
        };
        if let Some(Target::Entity(t)) = entity_path.last().map(Segment::target) {
            self.check_path_visible(req, t, &names)?;
        }
        let Some((rec, t)) = self.owner(req, session, entity_path, first).await? else {
            return Ok(None);
        };
        let mut node = if self
            .inner
            .model
            .property(&t, first)
            .is_some_and(|p| p.collection.is_some())
        {
            let language = language(req, &self.inner.config.default_language);
            let ctx = self.query_context(req, &language, session);
            Node::Collection(read_collection(&ctx, session, &rec, &t, first).await?)
        } else {
            rec.get(first)
                .cloned()
                .unwrap_or(Node::Value(odatabase_edm::Value::Null))
        };
        for name in &names[1..] {
            node = match node {
                Node::Complex(Some(inner)) => inner
                    .get(name)
                    .cloned()
                    .unwrap_or(Node::Value(odatabase_edm::Value::Null)),
                _ => Node::Value(odatabase_edm::Value::Null),
            };
        }
        let id = entity_id(&self.inner.model, &t, &rec).unwrap_or_default();
        let context = format!("{id}/{}", names.join("/"));
        Ok(Some((node, context, rec)))
    }

    async fn property(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let Some((node, context, _)) = self.read_property(req, uri, session).await? else {
            return Err(ODataError::not_found("the entity does not exist"));
        };
        if matches!(
            node,
            Node::Value(odatabase_edm::Value::Null) | Node::Complex(None)
        ) {
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let w = self.writer(req, format);
        let mut body = Map::new();
        if format.metadata != crate::MetadataLevel::None {
            body.insert(
                "@odata.context".into(),
                format!("{}/$metadata#{context}", req.root).into(),
            );
        }
        match &node {
            Node::Complex(Some(_)) => {
                if let Json::Object(o) = w.node(&node) {
                    body.extend(o);
                }
            }
            other => {
                body.insert("value".into(), w.node(other));
            }
        }
        Ok(ODataResponse::json(
            StatusCode::OK,
            format.content_type(),
            &Json::Object(body),
        ))
    }

    async fn raw_value(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        if self.is_media_path(uri) {
            return self.read_media(req, uri, session).await;
        }
        let Some((node, _, _)) = self.read_property(req, uri, session).await? else {
            return Err(ODataError::not_found("the entity does not exist"));
        };
        match node {
            Node::Value(odatabase_edm::Value::Null) => {
                Ok(ODataResponse::new(StatusCode::NO_CONTENT))
            }
            Node::Value(odatabase_edm::Value::Binary(b)) => Ok(ODataResponse::with_body(
                StatusCode::OK,
                "application/octet-stream",
                b,
            )),
            Node::Value(v) => Ok(ODataResponse::with_body(
                StatusCode::OK,
                "text/plain",
                v.to_text(),
            )),
            _ => Err(ODataError::bad_request("$value needs a primitive value")),
        }
    }

    async fn read_media(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let path = &uri.path[..uri.path.len() - 1];
        let Some(Target::Entity(t)) = path.last().map(Segment::target) else {
            return Err(ODataError::bad_request("$value needs an entity"));
        };
        let media = self
            .inner
            .model
            .type_chain(t)
            .into_iter()
            .find_map(|e| e.media.as_ref())
            .ok_or_else(|| ODataError::bad_request(format!("{t} has no stream")))?;
        let mut columns = vec![media.content_column.clone()];
        columns.extend(media.content_type_column.clone());
        let language = language(req, &self.inner.config.default_language);
        let ctx = self.query_context(req, &language, session);
        let row = read_columns(&ctx, session, path, t, &columns)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let body = match row.first() {
            Some(odatabase_edm::Value::Binary(b)) => b.clone(),
            _ => return Ok(ODataResponse::new(StatusCode::NO_CONTENT)),
        };
        let stored = row.get(1).and_then(odatabase_edm::Value::as_str);
        let mime = stored.unwrap_or(media.content_type.as_str());
        let mut r = ODataResponse::new(StatusCode::OK);
        let content_type = HeaderValue::try_from(mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream"));
        r.headers.insert(header::CONTENT_TYPE, content_type);
        r.body = body.into();
        Ok(r)
    }

    async fn references(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let path = &uri.path[..uri.path.len() - 1];
        let keys_only = QueryOptions {
            select: Some(Vec::new()),
            ..uri.options.clone()
        };
        let id = |r: &Record| {
            let e = entity_id(&self.inner.model, &r.type_name, r).unwrap_or_default();
            format!("{}/{e}", req.root)
        };
        match path.last().map(Segment::target) {
            Some(Target::EntityCollection(t)) => {
                let language = language(req, &self.inner.config.default_language);
                let ctx = self.query_context(req, &language, session);
                let records = read_entities(
                    &ctx,
                    session,
                    ReadRequest {
                        segments: path,
                        type_name: t,
                        options: &keys_only,
                        page: Page {
                            top: uri.options.top,
                            skip: uri.options.skip,
                        },
                        limits: self.limits(),
                    },
                )
                .await?;
                let value: Vec<Json> = records
                    .iter()
                    .map(|r| json!({"@odata.id": id(r)}))
                    .collect();
                let body = json!({
                    "@odata.context": format!("{}/$metadata#Collection($ref)", req.root),
                    "value": value,
                });
                Ok(ODataResponse::json(
                    StatusCode::OK,
                    format.content_type(),
                    &body,
                ))
            }
            Some(Target::Entity(t)) => {
                match self.read_one(req, session, path, t, &keys_only).await? {
                    None if matches!(path.last(), Some(Segment::Navigation { .. })) => {
                        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
                    }
                    None => Err(ODataError::not_found("the entity does not exist")),
                    Some(r) => {
                        let body = json!({
                            "@odata.context": format!("{}/$metadata#$ref", req.root),
                            "@odata.id": id(&r),
                        });
                        Ok(ODataResponse::json(
                            StatusCode::OK,
                            format.content_type(),
                            &body,
                        ))
                    }
                }
            }
            _ => Err(ODataError::bad_request("$ref needs entities")),
        }
    }
}

/// The entity type whose `Capabilities` restrictions apply to a read.
fn capability_type(uri: &ODataUri) -> Option<&String> {
    match uri.target() {
        Target::Count => uri
            .path
            .iter()
            .rev()
            .nth(1)
            .and_then(|s| target_type(s.target())),
        other => target_type(other),
    }
}

/// FNV-1a. Stable across builds, unlike the standard library's hasher.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn target_type(target: &Target) -> Option<&String> {
    match target {
        Target::Entity(t) | Target::EntityCollection(t) => Some(t),
        _ => None,
    }
}

/// `If-Match` style comparison: `*` matches everything, otherwise any listed tag.
pub(crate) fn etag_matches(header: &str, current: &str) -> bool {
    header.trim() == "*" || header.split(',').any(|t| t.trim() == current)
}
