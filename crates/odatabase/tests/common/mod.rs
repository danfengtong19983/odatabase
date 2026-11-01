//! The test service, modeled on the business partner schema of olingo-jpa-processor-v4.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{DateTime, NaiveDate, Utc};
use http_body_util::BodyExt;
use odatabase::{Database, Nav, ODataComplex, ODataEntity, ODataEnum, Service, ServiceBuilder};
use rust_decimal::Decimal;
use serde_json::Value as Json;
use sqlx::AssertSqlSafe;
use std::sync::OnceLock;
use testcontainers::ContainerAsync;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tower::ServiceExt;
use uuid::Uuid;

pub const SCHEMA: &str = include_str!("schema.sql");

#[derive(ODataEnum)]
#[odata(as_string)]
pub enum Abc {
    A = 1,
    B = 2,
    C = 3,
}

#[derive(ODataEnum)]
#[odata(flags)]
pub enum Access {
    None = 0,
    Read = 1,
    Write = 2,
}

#[derive(ODataComplex)]
pub struct Address {
    pub street: Option<String>,
    pub city: Option<String>,
    #[odata(max_length = 3)]
    pub country: Option<String>,
}

#[derive(ODataComplex)]
pub struct Phone {
    pub kind: Option<String>,
    pub number: Option<String>,
}

#[derive(ODataEntity)]
#[odata(table = "business_partner")]
pub struct BusinessPartner {
    #[odata(key)]
    pub id: String,
    #[odata(etag)]
    pub version: i64,
    pub name: Option<String>,
    pub rating: Option<i32>,
    pub revenue: Option<Decimal>,
    pub score: Option<f64>,
    pub active: bool,
    pub abc: Option<Abc>,
    pub flags: Option<Access>,
    pub created_at: Option<DateTime<Utc>>,
    pub founded: Option<NaiveDate>,
    pub external_id: Option<Uuid>,
    #[odata(embedded(prefix = "addr_"))]
    pub address: Address,
    #[odata(collection(
        table = "partner_comment",
        owner(id = "partner_id"),
        value_column = "body",
        order_column = "position"
    ))]
    pub comments: Vec<String>,
    #[odata(collection(
        table = "partner_phone",
        owner(id = "partner_id"),
        order_column = "position"
    ))]
    pub phones: Vec<Phone>,
    #[odata(has_many(target = Role, on(id = "partner_id"), partner = "Partner"))]
    pub roles: Nav,
    #[odata(has_many(
        target = Project,
        join_table(
            table = "partner_project",
            source(id = "partner_id"),
            target(project_id = "id")
        ),
        partner = "Partners"
    ))]
    pub projects: Nav,
    #[odata(belongs_to(target = BusinessPartner, on(owner_id = "id")))]
    pub owner: Nav,
    #[odata(has_many(target = BusinessPartner, on(id = "owner_id")))]
    pub owned: Nav,
}

#[derive(ODataEntity)]
#[odata(base = BusinessPartner, discriminator(column = "kind", value = "O"))]
pub struct Organization {
    pub size: Option<i32>,
}

#[derive(ODataEntity)]
#[odata(base = BusinessPartner, discriminator(column = "kind", value = "P"))]
pub struct Person {
    pub first_name: Option<String>,
}

#[derive(ODataEntity)]
#[odata(table = "partner_role")]
pub struct Role {
    #[odata(key)]
    pub partner_id: String,
    #[odata(key)]
    pub code: i32,
    pub label: Option<String>,
    #[odata(belongs_to(
        target = BusinessPartner,
        on(partner_id = "id"),
        required,
        partner = "Roles"
    ))]
    pub partner: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "project")]
pub struct Project {
    #[odata(key)]
    pub id: i32,
    pub title: String,
    #[odata(has_many(
        target = BusinessPartner,
        join_table(
            table = "partner_project",
            source(id = "project_id"),
            target(partner_id = "id")
        ),
        partner = "Projects"
    ))]
    pub partners: Nav,
    #[odata(has_many(target = Ticket, on(id = "project_id")))]
    pub tickets: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "division")]
pub struct Division {
    #[odata(key)]
    pub code: String,
    pub parent_code: Option<String>,
    pub population: Option<i64>,
    #[odata(belongs_to(target = Division, on(parent_code = "code")))]
    pub parent: Nav,
    #[odata(has_many(target = Division, on(code = "parent_code")))]
    pub children: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "settings", singleton = "Settings")]
pub struct Setting {
    #[odata(key)]
    pub id: i32,
    pub theme: Option<String>,
}

#[derive(ODataEntity)]
#[odata(table = "ticket")]
pub struct Ticket {
    #[odata(key, computed)]
    pub id: i32,
    #[odata(max_length = 20)]
    pub title: String,
    pub note: Option<String>,
    pub priority: Option<i32>,
    #[odata(etag)]
    pub version: i32,
    pub project_id: Option<i32>,
    #[odata(collection(
        table = "ticket_tag",
        owner(id = "ticket_id"),
        value_column = "tag",
        order_column = "position"
    ))]
    pub tags: Vec<String>,
}

pub fn builder() -> ServiceBuilder {
    Service::builder("Test")
        .entity::<BusinessPartner>()
        .entity::<Organization>()
        .entity::<Person>()
        .entity::<Division>()
        .entity::<Setting>()
        .entity::<Ticket>()
}

/// An HTTP client for one service instance.
#[derive(Clone)]
pub struct App {
    pub router: Router,
}

pub struct Response {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(&self) -> Json {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl App {
    pub fn new(service: Service) -> Self {
        Self {
            router: odatabase::mount("/odata", service),
        }
    }

    pub async fn send(&self, req: Request<Body>) -> Response {
        let response = self.router.clone().oneshot(req).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes().to_vec();
        Response {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    pub async fn request(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/odata/{}", encode_spaces(uri)))
            .header(header::HOST, "test.local");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        self.send(req.body(Body::from(body.to_owned())).unwrap())
            .await
    }

    pub async fn get(&self, uri: &str) -> Response {
        self.request(Method::GET, uri, &[], "").await
    }

    /// `GET` that must succeed with JSON.
    pub async fn ok(&self, uri: &str) -> Json {
        let r = self.get(uri).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.text());
        r.json()
    }

    pub async fn status(&self, uri: &str) -> u16 {
        self.get(uri).await.status.as_u16()
    }

    /// The values of `field` across `value`.
    pub async fn column(&self, uri: &str, field: &str) -> Vec<Json> {
        let doc = self.ok(uri).await;
        let items = doc["value"]
            .as_array()
            .unwrap_or_else(|| panic!("{uri}: {doc}"));
        items.iter().map(|i| i[field].clone()).collect()
    }

    /// The `Id`s of a partner query.
    pub async fn ids(&self, uri: &str) -> Vec<String> {
        self.column(uri, "Id")
            .await
            .into_iter()
            .map(|v| v.as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

fn encode_spaces(uri: &str) -> String {
    uri.replace(' ', "%20").replace('"', "%22")
}

pub async fn sqlite_pool(schema: &str) -> sqlx::SqlitePool {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .in_memory(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let schema = schema.replace("BYTEA", "BLOB").replace("SERIAL", "INTEGER");
    sqlx::raw_sql(AssertSqlSafe(schema))
        .execute(&pool)
        .await
        .unwrap();
    pool
}

pub async fn sqlite(build: impl FnOnce(Database) -> Service) -> App {
    App::new(build(sqlite_pool(SCHEMA).await.into()))
}

pub async fn sqlite_default() -> App {
    sqlite(|db| builder().build(db).unwrap()).await
}

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A running Postgres. Dropped at the end of the test that started it.
pub struct PostgresServer {
    _container: ContainerAsync<Postgres>,
    url: String,
}

impl PostgresServer {
    pub async fn start() -> Self {
        let container = Postgres::default()
            .with_tag("17-alpine")
            .start()
            .await
            .unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}");
        Self {
            _container: container,
            url,
        }
    }

    /// A fresh database with the test schema.
    pub async fn pool(&self, schema: &str) -> sqlx::PgPool {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let admin = sqlx::PgPool::connect(&format!("{}/postgres", self.url))
            .await
            .unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE test{n}")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        let pool = sqlx::PgPool::connect(&format!("{}/test{n}", self.url))
            .await
            .unwrap();
        sqlx::raw_sql(AssertSqlSafe(schema.to_owned()))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }
}

/// Whether a Docker daemon can run the Postgres suite.
pub fn docker_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Whether to run Postgres tests. Skipping is fine on a laptop without Docker, but in CI it
/// would turn a green build into a claim that was never tested, so it fails instead.
pub fn postgres_can_run() -> bool {
    let available = docker_available();
    assert!(
        available || std::env::var_os("CI").is_none(),
        "CI needs a Docker daemon: the Postgres tests must not be skipped"
    );
    if !available {
        eprintln!("Docker is unavailable: Postgres scenarios were NOT run");
    }
    available
}

/// Declares scenarios that run on SQLite as individual tests, and together on Postgres.
#[macro_export]
macro_rules! scenarios {
    ($build:expr; $($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let build = $build;
                let app = common::sqlite(build).await;
                super::$name(app).await;
            }
        )*

        #[tokio::test]
        async fn on_postgres() {
            if !common::postgres_can_run() {
                return;
            }
            let server = common::PostgresServer::start().await;
            $(
                let build = $build;
                let pool = server.pool(common::SCHEMA).await;
                let app = common::App::new(build(pool.into()));
                eprintln!("postgres: {}", stringify!($name));
                super::$name(app).await;
            )*
        }
    };
}
