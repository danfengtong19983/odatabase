use crate::change::{AcceptAll, ChangeHandler};
use crate::paging::{OffsetPaging, PagingProvider};
use crate::{ODataError, ODataRequest, ODataResponse};
use http::Method;
use odatabase_edm::{EdmEntity, Model, ModelBuilder, ModelError};
use odatabase_sql::{Database, ExpandLimits, QueryExtension};
use odatabase_uri::Limits;
use std::collections::HashMap;
use std::sync::Arc;

/// Service-wide settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub uri_limits: Limits,
    pub expand_limits: ExpandLimits,
    /// Language of description properties when the request names none.
    pub default_language: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            uri_limits: Limits::default(),
            expand_limits: ExpandLimits::default(),
            default_language: "en".to_owned(),
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
}

impl Service {
    pub fn builder(namespace: impl Into<String>) -> ServiceBuilder {
        ServiceBuilder {
            model: ModelBuilder::new(namespace),
            config: Config::default(),
            paging: Arc::new(OffsetPaging::default()),
            extensions: HashMap::new(),
            changes: Arc::new(AcceptAll),
        }
    }

    pub fn model(&self) -> &Model {
        &self.inner.model
    }

    /// Answers one request. Failures become OData error responses.
    pub async fn handle(&self, req: ODataRequest) -> ODataResponse {
        let mut response = match self.dispatch(&req).await {
            Ok(r) => r,
            Err(e) => e.to_response(),
        };
        response
            .headers
            .insert("odata-version", http::HeaderValue::from_static("4.0"));
        response
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
        let uri = odatabase_uri::parse_with(
            &self.inner.model,
            &req.path,
            &req.query,
            self.inner.config.uri_limits,
        )?;
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

    /// Replaces the pager. The default applies no server-side limit.
    pub fn paging(mut self, paging: impl PagingProvider + 'static) -> Self {
        self.paging = Arc::new(paging);
        self
    }

    /// Registers hooks that run around writes.
    pub fn change_handler(mut self, handler: impl ChangeHandler + 'static) -> Self {
        self.changes = Arc::new(handler);
        self
    }

    /// Validates the model and binds it to `database`.
    pub fn build(self, database: impl Into<Database>) -> Result<Service, ModelError> {
        let model = self.model.build()?;
        Ok(Service {
            inner: Arc::new(Inner {
                model,
                database: database.into(),
                config: self.config,
                paging: self.paging,
                extensions: self.extensions,
                changes: self.changes,
            }),
        })
    }
}
