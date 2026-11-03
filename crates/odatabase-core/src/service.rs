use crate::change::{AcceptAll, ChangeHandler};
use crate::operation::{OperationBuilder, OperationHandler};
use crate::paging::{OffsetPaging, PagingProvider};
use crate::transient::TransientCalculator;
use crate::{ODataError, ODataRequest, ODataResponse};
use http::Method;
use odatabase_edm::{Annotation, AnnotationValue, EdmEntity, Model, ModelBuilder, ModelError};
use odatabase_sql::{Database, ExpandLimits, QueryExtension};
use odatabase_uri::Limits;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Service-wide settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub uri_limits: Limits,
    pub expand_limits: ExpandLimits,
    /// Language of description properties when the request names none.
    pub default_language: String,
    /// The version `$schemaversion` selects. Any other value but `*` is a 404.
    pub schema_version: Option<String>,
    /// Time to answer a request. It frees the request, but a statement already running keeps
    /// going in the database: see `PostgresOptions::statement_timeout`.
    pub request_timeout: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            uri_limits: Limits::default(),
            expand_limits: ExpandLimits::default(),
            default_language: "en".to_owned(),
            schema_version: None,
            request_timeout: Some(Duration::from_secs(60)),
        }
    }
}

pub(crate) struct Inner {
    pub model: Model,
    pub database: Database,
    pub config: Config,
    pub paging: Arc<dyn PagingProvider>,
    pub extensions: HashMap<String, Arc<dyn QueryExtension>>,
    pub changes: Arc<dyn ChangeHandler>,
    pub operations: HashMap<String, Arc<dyn OperationHandler>>,
    pub calculators: HashMap<(String, String), Arc<dyn TransientCalculator>>,
}

/// An OData service: a model bound to a database. Cheap to clone.
#[derive(Clone)]
pub struct Service {
    pub(crate) inner: Arc<Inner>,
}

/// Declares the content of a [`Service`].
pub struct ServiceBuilder {
    model: ModelBuilder,
    config: Config,
    paging: Arc<dyn PagingProvider>,
    extensions: HashMap<String, Arc<dyn QueryExtension>>,
    changes: Arc<dyn ChangeHandler>,
    operations: HashMap<String, Arc<dyn OperationHandler>>,
    calculators: HashMap<(String, String), Arc<dyn TransientCalculator>>,
}

impl Service {
    pub fn builder(namespace: impl Into<String>) -> ServiceBuilder {
        ServiceBuilder {
            model: ModelBuilder::new(namespace),
            config: Config::default(),
            paging: Arc::new(OffsetPaging::default()),
            extensions: HashMap::new(),
            changes: Arc::new(AcceptAll),
            operations: HashMap::new(),
            calculators: HashMap::new(),
        }
    }

    pub fn model(&self) -> &Model {
        &self.inner.model
    }

    pub fn database(&self) -> &Database {
        &self.inner.database
    }

    /// Answers one request. Failures become OData error responses.
    pub async fn handle(&self, req: ODataRequest) -> ODataResponse {
        let outcome = match self.inner.config.request_timeout {
            Some(limit) => tokio::time::timeout(limit, self.dispatch(&req))
                .await
                .unwrap_or_else(|_| Err(ODataError::new(503, "the request took too long"))),
            None => self.dispatch(&req).await,
        };
        let mut response = outcome.unwrap_or_else(|e| e.to_response());
        response
            .headers
            .insert("odata-version", http::HeaderValue::from_static("4.0"));
        response
    }

    pub(crate) fn parse_uri(
        &self,
        req: &ODataRequest,
    ) -> Result<odatabase_uri::ODataUri, ODataError> {
        let path = req.path.trim_matches('/');
        let entity = path
            .strip_prefix("$entity")
            .or(path.strip_prefix("%24entity"));
        let cast = match entity {
            Some("") => Some(""),
            Some(rest) => rest.strip_prefix('/'),
            None => None,
        };
        let Some(cast) = cast else {
            return Ok(odatabase_uri::parse_with(
                &self.inner.model,
                &req.path,
                &req.query,
                self.inner.config.uri_limits,
            )?);
        };
        // `$entity?$id=url` is a read of that URL, with the rest of the query applied to it.
        if req.method != Method::GET {
            return Err(ODataError::new(405, "$entity can only be read"));
        }
        let (id, query) = odatabase_uri::take_entity_id(&req.query)?;
        let id = id.ok_or_else(|| ODataError::bad_request("$entity needs $id"))?;
        let relative = id.strip_prefix(&req.root).unwrap_or(&id);
        if relative.contains("://") || relative.contains('?') {
            return Err(ODataError::bad_request(format!(
                "{id} is not an entity of this service"
            )));
        }
        let relative = relative.trim_matches('/');
        let target = match cast {
            "" => relative.to_owned(),
            cast => format!("{relative}/{cast}"),
        };
        let uri = odatabase_uri::parse_with(
            &self.inner.model,
            &target,
            &query,
            self.inner.config.uri_limits,
        )?;
        if !matches!(uri.target(), odatabase_uri::Target::Entity(_)) {
            return Err(ODataError::bad_request(format!("{id} is not an entity")));
        }
        Ok(uri)
    }

    async fn dispatch(&self, req: &ODataRequest) -> Result<ODataResponse, ODataError> {
        if let Some(v) = req
            .headers
            .get("odata-maxversion")
            .and_then(|v| v.to_str().ok())
            && v.trim()
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok())
                .is_some_and(|m| m < 4)
        {
            return Err(ODataError::bad_request("the service implements OData 4.0"));
        }
        if let Some(response) = self.whole_service(req).await {
            return response;
        }
        let uri = self.parse_uri(req)?;
        if let Some(version) = &uri.options.schemaversion
            && version != "*"
            && self.inner.config.schema_version.as_ref() != Some(version)
        {
            return Err(ODataError::not_found(format!(
                "schema version {version} does not exist"
            )));
        }
        if *uri.target() == odatabase_uri::Target::Batch && req.method == Method::POST {
            return self.batch(req).await;
        }
        match req.method {
            Method::GET => self.get(req, &uri).await,
            Method::POST | Method::PATCH | Method::PUT | Method::DELETE => {
                self.modify(req, &uri).await
            }
            _ => Err(ODataError::new(
                405,
                format!("{} is not allowed here", req.method),
            )),
        }
    }
}

impl ServiceBuilder {
    pub fn entity<T: EdmEntity>(mut self) -> Self {
        self.model = self.model.entity::<T>();
        self
    }

    pub fn container(mut self, name: impl Into<String>) -> Self {
        self.model = self.model.container(name);
        self
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Replaces the pager. The default is [`OffsetPaging`] with `DEFAULT_PAGE_SIZE`.
    pub fn paging(mut self, paging: impl PagingProvider + 'static) -> Self {
        self.paging = Arc::new(paging);
        self
    }

    /// Registers hooks that run around writes.
    pub fn change_handler(mut self, handler: impl ChangeHandler + 'static) -> Self {
        self.changes = Arc::new(handler);
        self
    }

    /// Registers the calculator of transient property `property` of `T`.
    pub fn transient<T: EdmEntity>(
        mut self,
        property: &str,
        calculator: impl TransientCalculator + 'static,
    ) -> Self {
        let key = (T::edm_name().to_owned(), property.to_owned());
        self.calculators.insert(key, Arc::new(calculator));
        self
    }

    /// Adds an annotation that derive attributes cannot express, such as a record.
    pub fn annotate<T: EdmEntity>(mut self, annotation: Annotation) -> Self {
        self.model.register_entity::<T>();
        let name = T::edm_name();
        if let Some(t) = self.model.model_mut().entity_types.get_mut(name) {
            t.annotations.push(annotation);
        }
        self
    }

    /// Declares a function: side-effect free, called with `GET`.
    pub fn function(
        mut self,
        name: &str,
        define: impl FnOnce(OperationBuilder<'_>) -> OperationBuilder<'_>,
    ) -> Self {
        let (op, handler) = define(OperationBuilder::new(&mut self.model, name)).finish();
        self.operations
            .extend(handler.map(|h| (op.name.clone(), h)));
        self.model.add_function(op);
        self
    }

    /// Declares an action: may change data, called with `POST` inside a transaction.
    pub fn action(
        mut self,
        name: &str,
        define: impl FnOnce(OperationBuilder<'_>) -> OperationBuilder<'_>,
    ) -> Self {
        let (op, handler) = define(OperationBuilder::new(&mut self.model, name)).finish();
        self.operations
            .extend(handler.map(|h| (op.name.clone(), h)));
        self.model.add_action(op);
        self
    }

    /// Restricts every read of `T`, which must be declared `#[odata(query_extension)]`.
    pub fn query_extension<T: EdmEntity>(
        mut self,
        extension: impl QueryExtension + 'static,
    ) -> Self {
        self.extensions
            .insert(T::edm_name().to_owned(), Arc::new(extension));
        self
    }

    /// Validates the model and binds it to `database`.
    pub fn build(self, database: impl Into<Database>) -> Result<Service, ModelError> {
        let mut model = self.model.build()?;
        let transformations = odatabase_uri::APPLY_TRANSFORMATIONS
            .iter()
            .map(|t| AnnotationValue::String((*t).to_owned()))
            .collect();
        let supported = Annotation::new(
            "Aggregation.ApplySupported",
            AnnotationValue::Record(vec![(
                "Transformations".to_owned(),
                AnnotationValue::Collection(transformations),
            )]),
        );
        for set in model.entity_sets.values_mut() {
            if !set.annotations.iter().any(|a| a.term == supported.term) {
                set.annotations.push(supported.clone());
            }
        }
        let tracking = Annotation::new(
            "Capabilities.ChangeTracking",
            AnnotationValue::Record(vec![("Supported".to_owned(), AnnotationValue::Bool(true))]),
        );
        let tracked_sets: Vec<String> = model
            .entity_sets
            .values()
            .filter(|s| crate::delta::tracked(&model, &s.entity_type))
            .map(|s| s.name.clone())
            .collect();
        for set in tracked_sets {
            if let Some(s) = model.entity_sets.get_mut(&set) {
                s.annotations.push(tracking.clone());
            }
        }
        // An ordered collection takes `$index`.
        for t in model.entity_types.values_mut() {
            for p in &mut t.properties {
                if p.collection
                    .as_ref()
                    .is_some_and(|c| c.order_column.is_some())
                {
                    p.annotations.push(Annotation::new(
                        "Core.PositionalInsert",
                        AnnotationValue::Bool(true),
                    ));
                }
            }
        }
        for t in model.entity_types.values() {
            if crate::delta::tracked(&model, &t.name) {
                if crate::url::set_of(&model, &t.name).is_none() {
                    return Err(ModelError(format!(
                        "{}: tracking changes needs an entity set",
                        t.name
                    )));
                }
                if crate::write::row_restricted(&model, &t.name) {
                    return Err(ModelError(format!(
                        "{}: rows hidden from some callers cannot be tracked",
                        t.name
                    )));
                }
            }
            if t.query_extension && !self.extensions.contains_key(&t.name) {
                return Err(ModelError(format!(
                    "{}: no query extension registered",
                    t.name
                )));
            }
            for p in t.properties.iter().filter(|p| p.transient.is_some()) {
                if !self
                    .calculators
                    .contains_key(&(t.name.clone(), p.name.clone()))
                {
                    return Err(ModelError(format!("{}.{}: no calculator", t.name, p.name)));
                }
            }
        }
        Ok(Service {
            inner: Arc::new(Inner {
                model,
                database: database.into(),
                config: self.config,
                paging: self.paging,
                extensions: self.extensions,
                changes: self.changes,
                operations: self.operations,
                calculators: self.calculators,
            }),
        })
    }
}
