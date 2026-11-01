//! Functions and actions.

use crate::change::EntityKey;
use crate::format::Format;
use crate::json::Writer;
use crate::payload::{PropertyValue, read_property};
use crate::read::{language, split_entity_path};
use crate::{ODataError, ODataRequest, ODataResponse, Service};
use async_trait::async_trait;
use http::{Method, StatusCode, header};
use odatabase_edm::{
    EdmEntity, EdmField, FieldType, Model, ModelBuilder, Operation, OperationType,
    OperationTypeKind, Parameter, Property, TypeKind, Value,
};
use odatabase_sql::{Claims, ColumnDecode, Session, call_scalar, literal_args, lock_row};
use odatabase_uri::{Expr, ODataUri, QueryOptions, Segment, Target};
use serde_json::{Map, Value as Json};
use std::sync::Arc;

/// The entity or set a bound operation was called on.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    Entity { type_name: String, key: EntityKey },
    Collection { type_name: String },
}

/// What an operation handler receives.
pub struct OperationContext<'a> {
    pub model: &'a Model,
    pub claims: &'a Claims,
    /// A transaction for actions, the pool for functions.
    pub session: &'a Session,
    pub name: &'a str,
    pub parameters: Vec<(String, PropertyValue)>,
    pub binding: Option<Binding>,
}

impl OperationContext<'_> {
    /// A primitive parameter by name.
    pub fn value(&self, name: &str) -> Option<&Value> {
        self.parameters
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| match v {
                PropertyValue::Value(v) => Some(v),
                _ => None,
            })
    }
}

/// The outcome of an operation, which must match its declared return type.
#[derive(Clone, Debug, PartialEq)]
pub enum OperationResult {
    Void,
    Value(Value),
    Values(Vec<Value>),
    /// A complex value or a collection of them, as OData JSON.
    Json(Json),
    /// Entities by key. They are read back under the caller's restrictions.
    Keys(Vec<EntityKey>),
}

/// The application code behind an operation, like a `@EdmFunction` Java method.
///
/// `If-Match` is checked before a handler runs on a bound entity. A handler that changes the
/// entity must advance its version as well, or the old ETag stays valid.
#[async_trait]
pub trait OperationHandler: Send + Sync {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError>;
}

/// Declares an operation's signature.
pub struct OperationBuilder<'m> {
    model: &'m mut ModelBuilder,
    op: Operation,
    handler: Option<Arc<dyn OperationHandler>>,
}

fn to_type(f: FieldType) -> OperationType {
    OperationType {
        kind: OperationTypeKind::Value(f.kind),
        collection: f.collection,
        nullable: f.nullable,
    }
}

impl<'m> OperationBuilder<'m> {
    pub(crate) fn new(model: &'m mut ModelBuilder, name: &str) -> Self {
        Self {
            model,
            op: Operation {
                name: name.to_owned(),
                ..Operation::default()
            },
            handler: None,
        }
    }

    pub(crate) fn finish(self) -> (Operation, Option<Arc<dyn OperationHandler>>) {
        (self.op, self.handler)
    }

    /// Binds the operation to an entity type.
    pub fn bound_to<T: EdmEntity>(mut self) -> Self {
        self.bind::<T>(false);
        self
    }

    /// Binds the operation to a collection of an entity type.
    pub fn bound_to_collection<T: EdmEntity>(mut self) -> Self {
        self.bind::<T>(true);
        self
    }

    fn bind<T: EdmEntity>(&mut self, collection: bool) {
        self.model.register_entity::<T>();
        self.op.is_bound = true;
        self.op.parameters.insert(
            0,
            Parameter {
                name: "bindingParameter".to_owned(),
                ty: OperationType {
                    kind: OperationTypeKind::Entity(T::edm_name().to_owned()),
                    collection,
                    nullable: false,
                },
            },
        );
    }

    pub fn param<T: EdmField>(mut self, name: &str) -> Self {
        let ty = to_type(T::field_type(self.model));
        self.op.parameters.push(Parameter {
            name: name.to_owned(),
            ty,
        });
        self
    }

    pub fn returns<T: EdmField>(mut self) -> Self {
        self.op.return_type = Some(to_type(T::field_type(self.model)));
        self
    }

    pub fn returns_entity<T: EdmEntity>(self) -> Self {
        self.entity_result::<T>(false)
    }

    pub fn returns_entities<T: EdmEntity>(self) -> Self {
        self.entity_result::<T>(true)
    }

    fn entity_result<T: EdmEntity>(mut self, collection: bool) -> Self {
        self.model.register_entity::<T>();
        self.op.return_type = Some(OperationType {
            kind: OperationTypeKind::Entity(T::edm_name().to_owned()),
            collection,
            nullable: true,
        });
        self
    }

    /// Allows use in expressions and further path segments.
    pub fn composable(mut self) -> Self {
        self.op.is_composable = true;
        self
    }

    /// Runs a database function of this name instead of a handler.
    pub fn database_function(mut self, name: &str) -> Self {
        self.op.database_function = Some(name.to_owned());
        self
    }

    pub fn visible_for(mut self, group: &str) -> Self {
        self.op.visible_for.push(group.to_owned());
        self
    }

    pub fn handler(mut self, handler: impl OperationHandler + 'static) -> Self {
        self.handler = Some(Arc::new(handler));
        self
    }
}

impl Service {
    /// True when the entities of a function can be queried with plain SQL.
    pub(crate) fn composable_result(&self, uri: &ODataUri) -> bool {
        let Some(Segment::Function { name, overload, .. }) = uri.path.last() else {
            return false;
        };
        let op = &self.inner.model.functions[name][*overload];
        op.database_function.is_some()
            && matches!(
                uri.target(),
                Target::Entity(_) | Target::EntityCollection(_)
            )
    }

    pub(crate) async fn call_function(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let Some(Segment::Function {
            name,
            overload,
            args,
            ..
        }) = uri.path.last()
        else {
            return Err(ODataError::new(500, "not a function"));
        };
        let op = &self.inner.model.functions[name][*overload];
        if !req.claims.sees(&op.visible_for) {
            return Err(ODataError::new(403, format!("{name} is not accessible")));
        }
        let result = if let Some(db_name) = &op.database_function {
            self.binding(req, uri, session).await?;
            let values = literal_args(name, op, args)?;
            let ret = op
                .return_type
                .as_ref()
                .ok_or_else(|| ODataError::new(500, "a database function must return a value"))?;
            let decode = match &ret.kind {
                OperationTypeKind::Value(TypeKind::Primitive(k)) => ColumnDecode::Primitive(*k),
                OperationTypeKind::Value(TypeKind::Enum(n)) => ColumnDecode::Enum(n.clone()),
                _ => return Err(ODataError::not_implemented("structured database results")),
            };
            let lang = language(req, &self.inner.config.default_language);
            let ctx = self.query_context(req, &lang, session);
            OperationResult::Value(call_scalar(&ctx, session, db_name, &values, decode).await?)
        } else {
            let parameters = args
                .iter()
                .map(|(n, e)| match e {
                    Expr::Literal(v) => Ok((n.clone(), PropertyValue::Value(v.clone()))),
                    _ => Err(ODataError::bad_request(format!("{n} must be a literal"))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.run_handler(req, uri, name, parameters, session)
                .await?
        };
        self.operation_response(req, uri, op, result, session).await
    }

    pub(crate) async fn call_action(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let Some(Segment::Action { name, overload, .. }) = uri.path.last() else {
            return Err(ODataError::new(500, "not an action"));
        };
        let op = &self.inner.model.actions[name][*overload];
        if !req.claims.sees(&op.visible_for) {
            return Err(ODataError::new(403, format!("{name} is not accessible")));
        }
        let params = &op.parameters[usize::from(op.is_bound)..];
        let body: Map<String, Json> = if req.body.is_empty() {
            Map::new()
        } else {
            serde_json::from_slice(&req.body)
                .map_err(|e| ODataError::bad_request(format!("invalid JSON: {e}")))?
        };
        // Clients send annotations such as `a@odata.type` next to the parameters.
        let unknown = body
            .keys()
            .filter(|k| !k.contains('@'))
            .find(|k| !params.iter().any(|p| p.name == **k));
        if let Some(unknown) = unknown {
            return Err(ODataError::bad_request(format!(
                "unknown parameter {unknown}"
            )));
        }
        let mut parameters = Vec::with_capacity(params.len());
        for p in params {
            let holder = parameter_property(p)?;
            let value = match body.get(&p.name) {
                Some(json) => read_property(&self.inner.model, &holder, json, 0)?,
                None if p.ty.nullable => PropertyValue::Value(Value::Null),
                None => return Err(ODataError::bad_request(format!("{} is required", p.name))),
            };
            parameters.push((p.name.clone(), value));
        }
        let result = if let Some(db_name) = &op.database_function {
            self.binding(req, uri, session).await?;
            let mut values = Vec::with_capacity(parameters.len());
            for (name, p) in &parameters {
                match p {
                    PropertyValue::Value(v) => values.push(v.clone()),
                    _ => return Err(ODataError::bad_request(format!("{name} must be primitive"))),
                }
            }
            let decode = match op.return_type.as_ref().map(|r| &r.kind) {
                Some(OperationTypeKind::Value(TypeKind::Primitive(k))) => {
                    ColumnDecode::Primitive(*k)
                }
                _ => ColumnDecode::Natural,
            };
            let lang = language(req, &self.inner.config.default_language);
            let ctx = self.query_context(req, &lang, session);
            let v = call_scalar(&ctx, session, db_name, &values, decode).await?;
            if op.return_type.is_some() {
                OperationResult::Value(v)
            } else {
                OperationResult::Void
            }
        } else {
            self.run_handler(req, uri, name, parameters, session)
                .await?
        };
        self.operation_response(req, uri, op, result, session).await
    }

    async fn run_handler(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        name: &str,
        parameters: Vec<(String, PropertyValue)>,
        session: &Session,
    ) -> Result<OperationResult, ODataError> {
        let handler = self
            .inner
            .operations
            .get(name)
            .ok_or_else(|| ODataError::not_implemented(format!("{name} has no handler")))?;
        let binding = self.binding(req, uri, session).await?;
        let ctx = OperationContext {
            model: &self.inner.model,
            claims: &req.claims,
            session,
            name,
            parameters,
            binding,
        };
        handler.call(&ctx).await
    }

    /// Resolves the path before the operation.
    async fn binding(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        session: &Session,
    ) -> Result<Option<Binding>, ODataError> {
        let path = &uri.path[..uri.path.len() - 1];
        match path.last().map(Segment::target) {
            None => Ok(None),
            Some(Target::EntityCollection(t)) => Ok(Some(Binding::Collection {
                type_name: t.clone(),
            })),
            Some(Target::Entity(t)) => {
                let (entity_path, _) = split_entity_path(path);
                // The lock keeps the tag current until the action's transaction ends.
                if req.method == Method::POST && req.headers.contains_key(header::IF_MATCH) {
                    let lang = language(req, &self.inner.config.default_language);
                    let ctx = self.query_context(req, &lang, session);
                    lock_row(&ctx, session, entity_path, t).await?;
                }
                let (key, _) = self.locate(req, session, entity_path, t).await?;
                Ok(Some(Binding::Entity {
                    type_name: t.clone(),
                    key,
                }))
            }
            _ => Err(ODataError::bad_request("an operation binds to entities")),
        }
    }

    async fn operation_response(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        op: &Operation,
        result: OperationResult,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let format = Format::negotiate(uri.options.format.as_deref(), &req.headers)?;
        let model = &self.inner.model;
        let w = Writer {
            model,
            root: &req.root,
            format,
        };
        let context = |suffix: String| format!("{}/$metadata#{suffix}", req.root);
        let type_name = op.return_type.as_ref().map(|t| t.type_name(model));
        let mut body = Map::new();
        let metadata = format.metadata != crate::MetadataLevel::None;
        match result {
            OperationResult::Void | OperationResult::Value(Value::Null) => {
                return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
            }
            OperationResult::Value(v) => {
                if metadata && let Some(t) = &type_name {
                    body.insert("@odata.context".into(), context(t.clone()).into());
                }
                body.insert("value".into(), w.value(&v));
            }
            OperationResult::Values(values) => {
                if metadata && let Some(t) = &type_name {
                    body.insert(
                        "@odata.context".into(),
                        context(format!("Collection({t})")).into(),
                    );
                }
                body.insert("value".into(), values.iter().map(|v| w.value(v)).collect());
            }
            OperationResult::Json(json) => match json {
                Json::Object(o) => {
                    if metadata && let Some(t) = &type_name {
                        body.insert("@odata.context".into(), context(t.clone()).into());
                    }
                    body.extend(o);
                }
                other => {
                    if metadata && let Some(t) = &type_name {
                        body.insert(
                            "@odata.context".into(),
                            context(format!("Collection({t})")).into(),
                        );
                    }
                    body.insert("value".into(), other);
                }
            },
            OperationResult::Keys(keys) => {
                return self
                    .entities_by_key(req, uri, op, keys, format, session)
                    .await;
            }
        }
        Ok(ODataResponse::json(
            StatusCode::OK,
            format.content_type(),
            &Json::Object(body),
        ))
    }

    async fn entities_by_key(
        &self,
        req: &ODataRequest,
        uri: &ODataUri,
        op: &Operation,
        keys: Vec<EntityKey>,
        format: Format,
        session: &Session,
    ) -> Result<ODataResponse, ODataError> {
        let ret = op
            .return_type
            .as_ref()
            .ok_or_else(|| ODataError::new(500, "the operation returns no entities"))?;
        let OperationTypeKind::Entity(t) = &ret.kind else {
            return Err(ODataError::new(
                500,
                "the operation does not return entities",
            ));
        };
        let o = &uri.options;
        if o.filter.is_some() || !o.orderby.is_empty() || o.top.is_some() || o.skip.is_some() {
            return Err(ODataError::not_implemented(
                "query options on the result of a handler operation",
            ));
        }
        let options = QueryOptions {
            select: o.select.clone(),
            expand: o.expand.clone(),
            ..QueryOptions::default()
        };
        let mut records = Vec::with_capacity(keys.len());
        for key in keys {
            let segs = [Segment::Key {
                values: key,
                target: Target::Entity(t.clone()),
            }];
            records.extend(self.read_one(req, session, &segs, t, &options).await?);
        }
        if !ret.collection {
            let Some(rec) = records.first() else {
                return Ok(ODataResponse::new(StatusCode::NO_CONTENT));
            };
            return Ok(self.entity_response(req, rec, t, &options, format, StatusCode::OK));
        }
        let w = Writer {
            model: &self.inner.model,
            root: &req.root,
            format,
        };
        let mut body = Map::new();
        if format.metadata != crate::MetadataLevel::None {
            body.insert(
                "@odata.context".into(),
                format!(
                    "{}/$metadata#Collection({})",
                    req.root,
                    self.inner.model.qualified(t)
                )
                .into(),
            );
        }
        let items: Vec<Json> = records
            .iter()
            .map(|r| Json::Object(w.entity(r, t)))
            .collect();
        body.insert("value".into(), items.into());
        Ok(ODataResponse::json(
            StatusCode::OK,
            format.content_type(),
            &Json::Object(body),
        ))
    }
}

/// A property with the type of a parameter, for reading it with the payload rules.
fn parameter_property(p: &Parameter) -> Result<Property, ODataError> {
    let OperationTypeKind::Value(kind) = &p.ty.kind else {
        return Err(ODataError::not_implemented("entity parameters"));
    };
    let mut holder = Property::new(p.name.clone(), "", kind.clone());
    holder.nullable = p.ty.nullable;
    if p.ty.collection {
        holder.collection = Some(odatabase_edm::CollectionTable {
            table: String::new(),
            owner: Vec::new(),
            value_column: None,
            order_column: None,
        });
    }
    Ok(holder)
}
