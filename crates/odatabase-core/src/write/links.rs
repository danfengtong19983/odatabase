//! Links between entities: `@odata.bind`, `$ref`, and join-table rows.

use super::*;

impl Service {
    /// The target columns of a bound navigation.
    pub(super) async fn bind_target(
        &self,
        req: &ODataRequest,
        session: &Session,
        nav: &NavigationProperty,
        url: &str,
    ) -> Result<Vec<Value>, ODataError> {
        let (segs, t) = self.parse_entity_url(req, url)?;
        let cols: Vec<String> = nav.join.iter().map(|p| p.target.clone()).collect();
        self.column_values(req, session, &segs, &t, &cols)
            .await?
            .ok_or_else(|| ODataError::bad_request(format!("{url} does not exist")))
    }

    /// Resolves an entity URL, absolute or relative to the service root.
    pub(super) fn parse_entity_url(
        &self,
        req: &ODataRequest,
        url: &str,
    ) -> Result<(Vec<Segment>, String), ODataError> {
        let relative = url.strip_prefix(req.root.as_str()).unwrap_or(url);
        let uri = odatabase_uri::parse(&self.inner.model, relative, "")?;
        match uri.target().clone() {
            Target::Entity(t) => Ok((uri.path, t)),
            _ => Err(ODataError::bad_request(format!("{url} is not an entity"))),
        }
    }

    pub(super) async fn link_existing(
        &self,
        req: &ODataRequest,
        session: &Session,
        source: &[Segment],
        source_type: &str,
        nav: &NavigationProperty,
        url: &str,
    ) -> Result<(), ODataError> {
        let model = &self.inner.model;
        let (segs, t) = self.parse_entity_url(req, url)?;
        if let Some(jt) = &nav.join_table {
            let target_key = self.entity_key(req, session, &segs, &t).await?;
            return self
                .insert_link(req, session, source, source_type, jt, &t, &target_key)
                .await;
        }
        // Linking writes the foreign key on the target row.
        crate::capabilities::check_write(model, &t, &Method::PATCH)?;
        let cols: Vec<String> = nav.join.iter().map(|p| p.source.clone()).collect();
        let values = self
            .column_values(req, session, source, source_type, &cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let sets: Vec<ColumnWrite> = nav
            .join
            .iter()
            .zip(values)
            .map(|(p, v)| ColumnWrite::new(p.target.clone(), v, None))
            .collect();
        let key = self.entity_key(req, session, &segs, &t).await?;
        let l = layout(model, &t)?;
        let conditions = key_conditions(model, &req.claims, &t, &key)?;
        sql_update(model, session, &l.root, &sets, None, &conditions).await?;
        self.record(req, session, &t, &key, ChangeKind::Updated)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn insert_link(
        &self,
        req: &ODataRequest,
        session: &Session,
        source: &[Segment],
        source_type: &str,
        jt: &odatabase_edm::JoinTable,
        target_type: &str,
        target_key: &EntityKey,
    ) -> Result<(), ODataError> {
        let cols: Vec<String> = jt.source.iter().map(|p| p.source.clone()).collect();
        let source_values = self
            .column_values(req, session, source, source_type, &cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let cols: Vec<String> = jt.target.iter().map(|p| p.target.clone()).collect();
        let segs = [key_segment(target_type, target_key)];
        let target_values = self
            .column_values(req, session, &segs, target_type, &cols)
            .await?
            .ok_or_else(|| ODataError::not_found("the target does not exist"))?;
        let mut row = Vec::new();
        for (p, v) in jt.source.iter().zip(source_values) {
            row.push(ColumnWrite::new(p.target.clone(), v, None));
        }
        for (p, v) in jt.target.iter().zip(target_values) {
            row.push(ColumnWrite::new(p.source.clone(), v, None));
        }
        row.extend(tenant_cell(
            &self.inner.model,
            &req.claims,
            source_type,
            jt.tenant_column.as_ref(),
        )?);
        sql_insert(&self.inner.model, session, &jt.table, &row, &[]).await?;
        Ok(())
    }

    pub(super) async fn entity_key(
        &self,
        req: &ODataRequest,
        session: &Session,
        path: &[Segment],
        type_name: &str,
    ) -> Result<EntityKey, ODataError> {
        let model = &self.inner.model;
        let columns = key_columns(model, type_name)?;
        let values = self
            .column_values(req, session, path, type_name, &columns)
            .await?
            .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
        let mut key: EntityKey = model
            .key_of(type_name)
            .iter()
            .cloned()
            .zip(values)
            .collect();
        for (name, value) in &mut key {
            if let Some(TypeKind::Primitive(k)) = model.property(type_name, name).map(|p| &p.kind) {
                *value = fit(value, *k);
            }
        }
        Ok(key)
    }

    /// The owner and navigation of a `.../Nav/$ref` path.
    pub(super) fn ref_target<'u>(&'u self, uri: &'u ODataUri) -> Result<RefTarget<'u>, ODataError> {
        let path = &uri.path[..uri.path.len() - 1];
        let i = path
            .iter()
            .rposition(|s| matches!(s, Segment::Navigation { .. }))
            .ok_or_else(|| ODataError::bad_request("$ref needs a navigation property"))?;
        let Segment::Navigation { name, .. } = &path[i] else {
            unreachable!("found by position");
        };
        let owner = match i.checked_sub(1).map(|j| path[j].target()) {
            Some(Target::Entity(t)) => t.clone(),
            _ => return Err(ODataError::bad_request("$ref needs a single entity")),
        };
        let nav = self
            .inner
            .model
            .navigation(&owner, name)
            .ok_or_else(|| ODataError::new(500, "unknown navigation"))?;
        Ok((&path[..i], owner, nav, path.get(i + 1)))
    }

    pub(super) fn ref_body(body: &[u8]) -> Result<String, ODataError> {
        let doc: Json = serde_json::from_slice(body)
            .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
        doc.get("@odata.id")
            .or(doc.get("@id"))
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ODataError::bad_request("expected @odata.id"))
    }

    pub(super) async fn add_link(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let (source, owner, nav, _) = self.ref_target(uri)?;
        if !nav.collection {
            return Err(ODataError::new(
                405,
                "use PUT for a single-valued navigation",
            ));
        }
        let url = Self::ref_body(&req.body)?;
        self.link_existing(req, session, source, &owner, nav, &url)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    pub(super) async fn set_link(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (source, owner, nav, _) = self.ref_target(uri)?;
        if nav.collection {
            return Err(ODataError::new(405, "use POST for a collection navigation"));
        }
        let url = Self::ref_body(&req.body)?;
        if !fk_on_source(model, &owner, nav) {
            self.link_existing(req, session, source, &owner, nav, &url)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let values = self.bind_target(req, session, nav, &url).await?;
        let (key, version) = self.locate(req, session, source, &owner).await?;
        let mut writes = TableWrites::default();
        let l = layout(model, &owner)?;
        for (pair, value) in nav.join.iter().zip(values) {
            writes.push(&l.root, ColumnWrite::new(pair.source.clone(), value, None));
        }
        self.apply_update(req, session, &owner, &key, version, writes, false)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    pub(super) async fn remove_link(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (source, owner, nav, keyed) = self.ref_target(uri)?;
        let target_url = uri
            .options
            .custom
            .iter()
            .find(|(n, _)| n == "$id")
            .map(|(_, v)| v.clone());
        let target_segments: Option<(Vec<Segment>, String)> = match (keyed, &target_url) {
            (
                Some(Segment::Key {
                    values,
                    target: Target::Entity(t),
                }),
                _,
            ) => Some((
                vec![Segment::Key {
                    values: values.clone(),
                    target: Target::Entity(t.clone()),
                }],
                t.clone(),
            )),
            (_, Some(url)) => Some(self.parse_entity_url(req, url)?),
            _ => None,
        };
        if let Some(jt) = &nav.join_table {
            let (segs, t) = target_segments
                .ok_or_else(|| ODataError::bad_request("a target entity is required"))?;
            let cols: Vec<String> = jt.source.iter().map(|p| p.source.clone()).collect();
            let values = self
                .column_values(req, session, source, &owner, &cols)
                .await?
                .ok_or_else(|| ODataError::not_found("the entity does not exist"))?;
            let mut conditions: Vec<ColumnValue> = jt
                .source
                .iter()
                .map(|p| p.target.clone())
                .zip(values)
                .collect();
            let cols: Vec<String> = jt.target.iter().map(|p| p.target.clone()).collect();
            let values = self
                .column_values(req, session, &segs, &t, &cols)
                .await?
                .ok_or_else(|| ODataError::not_found("the target does not exist"))?;
            conditions.extend(jt.target.iter().map(|p| p.source.clone()).zip(values));
            conditions.extend(side_table_key(
                model,
                &req.claims,
                &owner,
                jt.tenant_column.as_ref(),
            )?);
            sql_delete(model, session, &jt.table, &conditions).await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let clear = |pairs: Vec<String>, table: &str, type_name: &str| -> TableWrites {
            let mut w = TableWrites::default();
            for c in pairs {
                let kind = column_kind(model, type_name, &c);
                w.push(table, ColumnWrite::new(c, Value::Null, kind));
            }
            w
        };
        if fk_on_source(model, &owner, nav) {
            let (key, version) = self.locate(req, session, source, &owner).await?;
            let l = layout(model, &owner)?;
            let cols = nav.join.iter().map(|p| p.source.clone()).collect();
            let writes = clear(cols, &l.root, &owner);
            self.apply_update(req, session, &owner, &key, version, writes, false)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let (segs, t) = target_segments
            .ok_or_else(|| ODataError::bad_request("a target entity is required"))?;
        let (key, version) = self.locate(req, session, &segs, &t).await?;
        let l = layout(model, &t)?;
        let cols = nav.join.iter().map(|p| p.target.clone()).collect();
        let writes = clear(cols, &l.root, &t);
        self.apply_update(req, session, &t, &key, version, writes, false)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }
}
