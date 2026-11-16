//! Writes of a single property, its raw `$value`, and media streams.

use super::*;

impl Service {
    /// True for `$value` of an entity rather than of a property.
    pub(crate) fn is_media_path(&self, uri: &ODataUri) -> bool {
        matches!(
            uri.path.iter().rev().nth(1).map(Segment::target),
            Some(Target::Entity(_))
        )
    }

    /// Stores or clears the stream of a media entity.
    pub(super) async fn write_media(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let path = &uri.path[..uri.path.len() - 1];
        let Some(Target::Entity(t)) = path.last().map(Segment::target) else {
            return Err(ODataError::bad_request("$value needs an entity"));
        };
        let media = model
            .type_chain(t)
            .into_iter()
            .find_map(|e| e.media.as_ref())
            .ok_or_else(|| ODataError::bad_request(format!("{t} has no stream")))?;
        let (key, version) = self.locate(req, session, path, t).await?;
        let l = layout(model, t)?;
        let mut writes = TableWrites::default();
        let binary = TypeKind::Primitive(PrimitiveKind::Binary);
        let content = if req.method == Method::DELETE {
            Value::Null
        } else {
            Value::Binary(req.body.to_vec())
        };
        writes.push(
            &l.root,
            ColumnWrite::new(media.content_column.clone(), content, Some(binary)),
        );
        if let Some(column) = &media.content_type_column {
            let mime = req
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map_or(Value::Null, |v| Value::String(v.to_owned()));
            let text = TypeKind::Primitive(PrimitiveKind::String);
            writes.push(&l.root, ColumnWrite::new(column.clone(), mime, Some(text)));
        }
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.apply_update(req, session, t, &key, version, writes, guarded)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    /// Splits `.../Set(key)/A/B[/$value]` into the owner and the property names.
    pub(super) fn property_target<'u>(
        &self,
        uri: &'u ODataUri,
    ) -> Result<(&'u [Segment], String, Vec<&'u str>), ODataError> {
        let path = match uri.path.last() {
            Some(Segment::Value) => &uri.path[..uri.path.len() - 1],
            _ => &uri.path[..],
        };
        let (entity_path, tail) = split_entity_path(path);
        let t = match entity_path.last().map(Segment::target) {
            Some(Target::Entity(t)) => t.clone(),
            _ => {
                return Err(ODataError::bad_request(
                    "the property needs a single entity",
                ));
            }
        };
        let names: Vec<&str> = tail
            .iter()
            .filter_map(|s| match s {
                Segment::Property { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        Ok((entity_path, t, names))
    }

    pub(super) async fn update_property(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (entity_path, t, names) = self.property_target(uri)?;
        self.check_path_visible(req, &t, &names)?;
        let (key, version) = self.locate(req, session, entity_path, &t).await?;
        let raw = matches!(uri.path.last(), Some(Segment::Value));
        let p = self.walk_property(&t, &names)?;
        let (prefix, leaf) = (p.0, p.1);
        if model.key_of(&t).iter().any(|k| names == [k.as_str()]) {
            return Err(ODataError::bad_request("the key cannot be changed"));
        }
        let value = if raw {
            let TypeKind::Primitive(k) = leaf.kind else {
                return Err(ODataError::bad_request("$value needs a primitive"));
            };
            let text = std::str::from_utf8(&req.body)
                .map_err(|_| ODataError::bad_request("the value is not text"))?;
            PropertyValue::Value(
                Value::parse_text(k, text).map_err(|e| ODataError::bad_request(e.to_string()))?,
            )
        } else {
            let doc: Json = serde_json::from_slice(&req.body)
                .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?;
            let body = if matches!(leaf.kind, TypeKind::Complex(_)) && leaf.collection.is_none() {
                match doc {
                    Json::Object(mut o) if o.contains_key("value") => {
                        o.remove("value").unwrap_or(Json::Null)
                    }
                    other => other,
                }
            } else {
                doc.get("value").cloned().ok_or_else(|| {
                    ODataError::bad_request("expected a JSON object with a value member")
                })?
            };
            read_property(model, leaf, &body, 0)?
        };
        if let PropertyValue::Collection(items) = &value {
            self.write_collection(req, session, &t, leaf, &key, items, true)
                .await?;
            self.apply_update(
                req,
                session,
                &t,
                &key,
                version,
                TableWrites::default(),
                false,
            )
            .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        let mut writes = TableWrites::default();
        let l = layout(model, &t)?;
        let owner = l
            .own
            .iter()
            .find(|o| o.properties.iter().any(|q| q.name == names[0]))
            .ok_or_else(|| ODataError::new(500, "unknown owner"))?;
        let table = table_of(&l, owner).to_owned();
        flatten(
            model,
            leaf,
            &prefix,
            &value,
            req.method == Method::PUT,
            writes.columns(&table),
        )?;
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.apply_update(req, session, &t, &key, version, writes, guarded)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }

    /// The column prefix and declaration of a property path.
    pub(super) fn walk_property(
        &self,
        type_name: &str,
        names: &[&str],
    ) -> Result<(String, &Property), ODataError> {
        let model = &self.inner.model;
        let first = names
            .first()
            .ok_or_else(|| ODataError::bad_request("no property"))?;
        let mut p = model
            .property(type_name, first)
            .ok_or_else(|| ODataError::bad_request(format!("unknown property {first}")))?;
        let mut prefix = String::new();
        for name in &names[1..] {
            let TypeKind::Complex(t) = &p.kind else {
                return Err(ODataError::bad_request(format!("{name} has no parent")));
            };
            prefix.push_str(&p.column);
            let c = model
                .complex_type(t)
                .ok_or_else(|| ODataError::new(500, format!("unknown complex type {t}")))?;
            p = c
                .properties
                .iter()
                .find(|i| i.name == *name)
                .ok_or_else(|| ODataError::bad_request(format!("unknown property {name}")))?;
        }
        Ok((prefix, p))
    }

    pub(super) async fn clear_property(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let model = &self.inner.model;
        let (entity_path, t, names) = self.property_target(uri)?;
        self.check_path_visible(req, &t, &names)?;
        let (key, version) = self.locate(req, session, entity_path, &t).await?;
        let (prefix, leaf) = self.walk_property(&t, &names)?;
        if leaf.collection.is_some() {
            self.write_collection(req, session, &t, leaf, &key, &[], true)
                .await?;
            self.record(req, session, &t, &key, ChangeKind::Updated)
                .await?;
            return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
        }
        if !leaf.nullable {
            return Err(ODataError::bad_request(format!(
                "{} cannot be null",
                leaf.name
            )));
        }
        let nulled = match leaf.kind {
            TypeKind::Complex(_) => PropertyValue::Complex(None),
            _ => PropertyValue::Value(Value::Null),
        };
        let l = layout(model, &t)?;
        let owner = l
            .own
            .iter()
            .find(|o| o.properties.iter().any(|q| q.name == names[0]))
            .ok_or_else(|| ODataError::new(500, "unknown owner"))?;
        let table = table_of(&l, owner).to_owned();
        let mut writes = TableWrites::default();
        flatten(model, leaf, &prefix, &nulled, true, writes.columns(&table))?;
        let guarded = req.headers.contains_key(header::IF_MATCH);
        self.apply_update(req, session, &t, &key, version, writes, guarded)
            .await?;
        Ok(ODataResponse::new(StatusCode::NO_CONTENT))
    }
}
