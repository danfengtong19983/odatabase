//! A full end-to-end run of a multi-tenant shop, over a real TCP socket and plain HTTP/1.1,
//! on SQLite and on PostgreSQL.
//!
//! The other suites send requests to the router in process. This one starts `axum::serve`
//! with authentication middleware in front, the way an application deploys the library, and
//! walks one client session through the whole feature set: discovery, deep inserts, queries,
//! paging, aggregation, concurrency control, links, batches, operations, change tracking,
//! tenant isolation, group visibility and error handling.
mod common;

use async_trait::async_trait;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use odatabase::edm::{Annotation, AnnotationValue, Value};
use odatabase::sql::{ColumnWrite, update};
use odatabase::{
    Binding, ChangeContext, ChangeHandler, ClaimValue, Claims, Database, EntityKey, EntityPayload,
    Nav, ODataEntity, ODataEnum, ODataError, OffsetPaging, OperationContext, OperationHandler,
    OperationResult, PropertyValue, Service,
};
use rust_decimal::Decimal;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------------------------

const SCHEMA: &str = "
CREATE TABLE category (code TEXT PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE customer (
  tenant_id TEXT NOT NULL, id TEXT NOT NULL, version BIGINT NOT NULL DEFAULT 1,
  name TEXT NOT NULL, email TEXT, street TEXT, city TEXT, joined DATE,
  PRIMARY KEY (tenant_id, id)
);
CREATE TABLE customer_tag (
  tenant_id TEXT NOT NULL, customer_id TEXT NOT NULL, position INTEGER NOT NULL, tag TEXT NOT NULL,
  PRIMARY KEY (tenant_id, customer_id, position)
);
CREATE TABLE customer_category (
  tenant_id TEXT NOT NULL, customer_id TEXT NOT NULL,
  category_code TEXT NOT NULL REFERENCES category (code),
  PRIMARY KEY (tenant_id, customer_id, category_code)
);
CREATE TABLE orders (
  id SERIAL PRIMARY KEY, tenant_id TEXT NOT NULL, customer_id TEXT NOT NULL,
  version BIGINT NOT NULL DEFAULT 1, status TEXT NOT NULL, total NUMERIC(12, 2) NOT NULL,
  placed_on DATE,
  FOREIGN KEY (tenant_id, customer_id) REFERENCES customer (tenant_id, id)
);
CREATE TABLE order_line (
  tenant_id TEXT NOT NULL, order_id INTEGER NOT NULL REFERENCES orders (id),
  line_no INTEGER NOT NULL, product TEXT NOT NULL, qty INTEGER NOT NULL,
  PRIMARY KEY (order_id, line_no)
);
INSERT INTO category (code, name) VALUES ('B2B', 'Business'), ('VIP', 'Very important'), ('NEW', 'New');
";

#[derive(odatabase::ODataComplex)]
pub struct Address {
    pub street: Option<String>,
    pub city: Option<String>,
}

#[derive(ODataEnum)]
#[odata(as_string)]
pub enum Status {
    Open = 1,
    Shipped = 2,
    Cancelled = 3,
}

#[derive(ODataEntity)]
#[odata(table = "category", set = "Categories")]
pub struct Category {
    #[odata(key)]
    pub code: String,
    pub name: String,
}

#[derive(ODataEntity)]
#[odata(table = "customer", track_changes)]
pub struct Customer {
    #[odata(tenant)]
    pub tenant_id: String,
    #[odata(key)]
    pub id: String,
    #[odata(etag)]
    pub version: i64,
    #[odata(max_length = 40, searchable)]
    pub name: String,
    #[odata(visible_for("staff"))]
    pub email: Option<String>,
    #[odata(embedded(prefix = ""))]
    pub address: Address,
    pub joined: Option<chrono::NaiveDate>,
    #[odata(collection(
        table = "customer_tag",
        owner(id = "customer_id"),
        value_column = "tag",
        order_column = "position",
        tenant_column = "tenant_id"
    ))]
    pub tags: Vec<String>,
    #[odata(has_many(target = Order, on(id = "customer_id"), partner = "Customer"))]
    pub orders: Nav,
    #[odata(has_many(
        target = Category,
        join_table(
            table = "customer_category",
            source(id = "customer_id"),
            target(category_code = "code"),
            tenant_column = "tenant_id"
        )
    ))]
    pub categories: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "orders", track_changes)]
pub struct Order {
    #[odata(tenant)]
    pub tenant_id: String,
    #[odata(key, computed)]
    pub id: i32,
    pub customer_id: String,
    #[odata(etag)]
    pub version: i64,
    pub status: Status,
    pub total: Decimal,
    pub placed_on: Option<chrono::NaiveDate>,
    #[odata(belongs_to(
        target = Customer,
        on(customer_id = "id"),
        required,
        partner = "Orders"
    ))]
    pub customer: Nav,
    #[odata(has_many(target = OrderLine, on(id = "order_id"), partner = "Order"))]
    pub lines: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "order_line")]
pub struct OrderLine {
    #[odata(tenant)]
    pub tenant_id: String,
    #[odata(key)]
    pub order_id: i32,
    #[odata(key)]
    pub line_no: i32,
    pub product: String,
    pub qty: i32,
    #[odata(belongs_to(target = Order, on(order_id = "id"), required, partner = "Lines"))]
    pub order: Nav,
}

/// Trims names and refuses lines without a positive quantity, nested or not.
struct Rules;

impl Rules {
    fn check(entity: &mut EntityPayload) -> Result<(), ODataError> {
        for (name, value) in &mut entity.values {
            match (name.as_str(), value) {
                ("Name", PropertyValue::Value(Value::String(s))) => *s = s.trim().to_owned(),
                ("Qty", PropertyValue::Value(v)) if v.as_i64().is_some_and(|q| q <= 0) => {
                    return Err(ODataError::bad_request("Qty must be positive"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ChangeHandler for Rules {
    async fn before_create(
        &self,
        _: &ChangeContext<'_>,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Self::check(entity)
    }

    async fn before_update(
        &self,
        _: &ChangeContext<'_>,
        _: &EntityKey,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        Self::check(entity)
    }
}

/// `POST Orders(n)/Shop.Cancel`: sets the status and advances the ETag.
struct Cancel;

#[async_trait]
impl OperationHandler for Cancel {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let Some(Binding::Entity { key, .. }) = &ctx.binding else {
            return Err(ODataError::bad_request("bound to an order"));
        };
        let Some([ClaimValue::Equals(tenant)]) = ctx.claims.values.get("tenant").map(Vec::as_slice)
        else {
            return Err(ODataError::new(403, "one tenant required"));
        };
        let changed = update(
            ctx.model,
            ctx.session,
            "orders",
            &[ColumnWrite::new(
                "status",
                Value::String("Cancelled".into()),
                None,
            )],
            Some("version"),
            &[
                ("id".to_owned(), key[0].1.clone()),
                ("tenant_id".to_owned(), tenant.clone()),
            ],
        )
        .await?;
        Ok(OperationResult::Value(Value::Boolean(changed == 1)))
    }
}

/// `Greet(name='x')`, an unbound function with an application handler.
struct Greet;

#[async_trait]
impl OperationHandler for Greet {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let name = ctx.value("name").map(Value::to_text).unwrap_or_default();
        Ok(OperationResult::Value(Value::String(format!(
            "Hello, {name}"
        ))))
    }
}

fn service(db: Database) -> Service {
    Service::builder("Shop")
        .entity::<Category>()
        .entity::<Customer>()
        .entity::<Order>()
        .entity::<OrderLine>()
        .annotate::<Category>(Annotation::new(
            "Capabilities.InsertRestrictions",
            AnnotationValue::Record(vec![(
                "Insertable".to_owned(),
                AnnotationValue::Bool(false),
            )]),
        ))
        .annotate::<Category>(Annotation::new(
            "Capabilities.UpdateRestrictions",
            AnnotationValue::Record(vec![("Updatable".to_owned(), AnnotationValue::Bool(false))]),
        ))
        .paging(OffsetPaging { page_size: Some(2) })
        .change_handler(Rules)
        .action("Cancel", |a| {
            a.bound_to::<Order>().returns::<bool>().handler(Cancel)
        })
        .function("Greet", |f| {
            f.param::<String>("name").returns::<String>().handler(Greet)
        })
        .function("Abs", |f| {
            f.param::<i32>("x")
                .returns::<i32>()
                .database_function("abs")
        })
        .build(db)
        .unwrap()
}

/// Stands in for real authentication: `X-Tenant` names the tenant, `X-Groups` the groups.
async fn authenticate(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    let headers = request.headers();
    let tenant = headers
        .get("x-tenant")
        .and_then(|h| h.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_owned();
    let groups = headers
        .get("x-groups")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let mut claims = Claims::default().with("tenant", ClaimValue::Equals(Value::String(tenant)));
    for group in groups.split(',').filter(|g| !g.is_empty()) {
        claims = claims.with_group(group);
    }
    request.extensions_mut().insert(claims);
    Ok(next.run(request).await)
}

// ---------------------------------------------------------------------------------------------
// A minimal HTTP/1.1 client, so that requests cross a real socket
// ---------------------------------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(db: Database) -> Server {
    db.create_change_log().await.unwrap();
    let app =
        odatabase::mount("/odata", service(db)).layer(axum::middleware::from_fn(authenticate));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { addr, task }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {} {}", self.status, self.text()))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// A client acting for one tenant with some groups.
#[derive(Clone)]
struct Client {
    addr: SocketAddr,
    tenant: Option<String>,
    groups: String,
}

/// Percent-encodes what a URL can't carry as is, leaving OData's own syntax readable.
fn encode(target: &str) -> String {
    let mut out = String::with_capacity(target.len());
    for b in target.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(b as char),
            b'-' | b'.' | b'_' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b','
            | b';' | b'=' | b':' | b'@' | b'/' | b'?' | b'%' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Client {
    fn new(addr: SocketAddr, tenant: &str) -> Self {
        Self {
            addr,
            tenant: Some(tenant.to_owned()),
            groups: String::new(),
        }
    }

    fn with_groups(&self, groups: &str) -> Self {
        Self {
            groups: groups.to_owned(),
            ..self.clone()
        }
    }

    /// Sends a request to a URL relative to the service root, or to an absolute link.
    async fn send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let target = match url.strip_prefix(&format!("http://{}", self.addr)) {
            Some(path) => path.to_owned(),
            None => format!("/odata/{url}"),
        };
        let mut head = format!(
            "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            encode(&target),
            self.addr,
            body.len()
        );
        if let Some(tenant) = &self.tenant {
            head.push_str(&format!("X-Tenant: {tenant}\r\n"));
        }
        if !self.groups.is_empty() {
            head.push_str(&format!("X-Groups: {}\r\n", self.groups));
        }
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        parse_reply(&raw)
    }

    async fn get(&self, url: &str) -> Reply {
        self.send("GET", url, &[], "").await
    }

    /// A `GET` that must succeed with JSON.
    async fn ok(&self, url: &str) -> Json {
        let r = self.get(url).await;
        assert_eq!(r.status, 200, "GET {url}: {}", r.text());
        r.json()
    }

    async fn status(&self, url: &str) -> u16 {
        self.get(url).await.status
    }

    async fn write(&self, method: &str, url: &str, headers: &[(&str, &str)], body: Json) -> Reply {
        let mut all = vec![("Content-Type", "application/json")];
        all.extend_from_slice(headers);
        self.send(method, url, &all, &body.to_string()).await
    }

    /// The values of `field` in a collection response.
    async fn column(&self, url: &str, field: &str) -> Vec<Json> {
        let doc = self.ok(url).await;
        doc["value"]
            .as_array()
            .unwrap_or_else(|| panic!("{url}: {doc}"))
            .iter()
            .map(|e| e[field].clone())
            .collect()
    }

    /// Every entity of a collection, following `@odata.nextLink` to the end.
    async fn all(&self, url: &str) -> (Vec<Json>, usize) {
        let (mut items, mut pages) = (Vec::new(), 0);
        let mut next = Some(url.to_owned());
        while let Some(url) = next.take() {
            let doc = self.ok(&url).await;
            pages += 1;
            items.extend(doc["value"].as_array().unwrap().iter().cloned());
            next = doc["@odata.nextLink"].as_str().map(str::to_owned);
            assert!(pages < 50, "paging does not end");
        }
        (items, pages)
    }
}

fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response head");
    let head = std::str::from_utf8(&raw[..split]).unwrap();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    assert!(status_line.starts_with("HTTP/1.1 "), "{status_line}");
    let status = status_line[9..12].parse().unwrap();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let rest = &raw[split + 4..];
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|t| t.contains("chunked"))
    {
        dechunk(rest)
    } else {
        let len = headers
            .get("content-length")
            .map_or(rest.len(), |l| l.parse().unwrap());
        assert_eq!(rest.len(), len, "body length matches Content-Length");
        rest.to_vec()
    };
    Reply {
        status,
        headers,
        body,
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n").unwrap();
        let size_text = std::str::from_utf8(&data[..eol]).unwrap();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16).unwrap();
        data = &data[eol + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

// ---------------------------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------------------------

/// Runs every stage against one fresh database, in order, as one client session would.
async fn shop(db: Database) {
    let server = serve(db).await;
    let acme = Client::new(server.addr, "acme");
    let staff = acme.with_groups("staff");
    let globex = Client::new(server.addr, "globex");

    discovery(&acme, server.addr).await;
    let orders = deep_insert(&staff).await;
    more_customers(&acme).await;
    queries(&acme, &orders).await;
    paths_and_values(&acme, &orders).await;
    search_and_compute(&acme).await;
    paging(&acme).await;
    aggregation(&acme).await;
    concurrency(&acme).await;
    links(&acme).await;
    batches(&acme).await;
    operations(&acme, &orders).await;
    tenants(&acme, &globex).await;
    visibility(&acme, &staff).await;
    change_tracking(&acme, &globex).await;
    errors(&acme).await;
    parallel_clients(&acme, &globex).await;
}

async fn discovery(acme: &Client, addr: SocketAddr) {
    let anonymous = Client {
        addr,
        tenant: None,
        groups: String::new(),
    };
    assert_eq!(
        anonymous.status("Customers").await,
        401,
        "the middleware runs before the service"
    );

    let r = acme.get("").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("odata-version"), Some("4.0"));
    let names: Vec<_> = r.json()["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    for set in ["Categories", "Customers", "Orders", "OrderLines"] {
        assert!(names.iter().any(|n| n == set), "{set} in {names:?}");
    }
    assert_eq!(acme.status("/").await, 200, "trailing slash form");

    let r = acme.get("$metadata").await;
    assert_eq!(r.status, 200);
    assert!(
        r.header("content-type")
            .unwrap()
            .contains("application/xml")
    );
    let xml = r.text();
    for needle in [
        r#"<EntityType Name="Customer""#,
        r#"<EnumType Name="Status""#,
        r#"<ComplexType Name="Address""#,
        r#"<Action Name="Cancel" IsBound="true""#,
        r#"<Function Name="Greet""#,
        "Capabilities.InsertRestrictions",
        "Capabilities.UpdateRestrictions",
    ] {
        assert!(xml.contains(needle), "{needle} in $metadata");
    }
    assert!(!xml.contains("TenantId"), "the tenant column stays hidden");
    let etag = r.header("etag").expect("$metadata has an ETag").to_owned();
    let r = acme
        .send("GET", "$metadata", &[("If-None-Match", &etag)], "")
        .await;
    assert_eq!(r.status, 304);

    let csdl = acme.ok("$metadata?$format=json").await;
    assert_eq!(csdl["$Version"], "4.0");
    assert_eq!(csdl["Shop"]["Order"]["Total"]["$Type"], "Edm.Decimal");
}

/// Creates a customer with orders and lines in one request. Returns the generated order ids.
async fn deep_insert(staff: &Client) -> Vec<i64> {
    let r = staff
        .write(
            "POST",
            "Customers",
            &[("Prefer", "return=representation")],
            json!({
                "Id": "c1",
                "Name": "  Ada Lovelace  ",
                "Email": "ada@example.org",
                "Address": {"Street": "1 Main St", "City": "London"},
                "Joined": "2024-03-01",
                "Tags": ["math", "poetry"],
                "Categories@odata.bind": ["Categories('VIP')"],
                "Orders": [
                    {"Status": "Open", "Total": 30.5, "PlacedOn": "2024-03-02",
                     "Lines": [{"LineNo": 1, "Product": "Pen", "Qty": 3},
                               {"LineNo": 2, "Product": "Ink", "Qty": 1}]},
                    {"Status": "Shipped", "Total": 12, "PlacedOn": "2024-04-10",
                     "Lines": [{"LineNo": 1, "Product": "Paper", "Qty": 10}]}
                ]
            }),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(
        r.header("preference-applied"),
        Some("return=representation")
    );
    assert!(
        r.header("location")
            .unwrap()
            .ends_with("/odata/Customers('c1')"),
        "{:?}",
        r.header("location")
    );
    assert_eq!(r.header("etag"), Some(r#"W/"1""#));
    let doc = r.json();
    assert_eq!(doc["Name"], "Ada Lovelace", "the change handler trimmed it");
    assert_eq!(doc["Tags"], json!(["math", "poetry"]));

    let doc = staff
        .ok("Customers('c1')?$expand=Orders($orderby=PlacedOn;$expand=Lines($orderby=LineNo)),Categories")
        .await;
    assert_eq!(doc["Categories"][0]["Code"], "VIP");
    let orders = doc["Orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0]["Lines"].as_array().unwrap().len(), 2);
    assert_eq!(orders[1]["Lines"][0]["Product"], "Paper");
    assert_eq!(orders[0]["Status"], "Open");

    // A rule in the change handler applies to nested lines, and stops the whole insert.
    let r = staff
        .write(
            "POST",
            "Customers",
            &[],
            json!({"Id": "bad", "Name": "Bad",
                   "Orders": [{"Status": "Open", "Total": 1,
                               "Lines": [{"LineNo": 1, "Product": "X", "Qty": 0}]}]}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(
        staff.status("Customers('bad')").await,
        404,
        "nothing was kept"
    );

    orders.iter().map(|o| o["Id"].as_i64().unwrap()).collect()
}

async fn more_customers(acme: &Client) {
    for (id, name, city, total) in [
        ("c2", "Grace Hopper", "New York", 99.0),
        ("c3", "Alan Turing", "London", 5.25),
        ("c4", "Edsger Dijkstra", "Austin", 42.0),
        ("c5", "Barbara Liskov", "Boston", 0.0),
    ] {
        let mut body = json!({"Id": id, "Name": name, "Address": {"City": city}});
        if total > 0.0 {
            body["Orders"] = json!([{"Status": "Open", "Total": total}]);
        }
        let r = acme.write("POST", "Customers", &[], body).await;
        assert_eq!(r.status, 201, "{id}: {}", r.text());
        assert!(r.body.is_empty() || r.json()["Id"] == id);
    }
    // POST below a navigation sets the foreign key from the path.
    let r = acme
        .write(
            "POST",
            "Customers('c2')/Orders",
            &[],
            json!({"Status": "Shipped", "Total": 1.5}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["CustomerId"], "c2");
}

async fn queries(acme: &Client, orders: &[i64]) {
    let doc = acme
        .ok("Customers?$filter=Address/City eq 'London'&$select=Id,Name&$orderby=Name desc&$count=true")
        .await;
    assert_eq!(doc["@odata.count"], 2);
    assert_eq!(doc["value"][0]["Name"], "Alan Turing");
    assert!(doc["value"][0].get("Address").is_none(), "$select applies");
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("$metadata#Customers(Id,Name)")
    );

    let ids = acme
        .column(
            "Customers?$filter=Orders/any(o: o/Total gt 40)&$orderby=Id",
            "Id",
        )
        .await;
    assert_eq!(ids, ["c2", "c4"]);
    let ids = acme
        .column(
            "Customers?$filter=Orders/all(o: o/Status eq Shop.Status'Open') and Orders/$count gt 0&$orderby=Id",
            "Id",
        )
        .await;
    assert_eq!(ids, ["c3", "c4"], "c2 also has a shipped order");
    let _ = orders;
}

fn number(v: &Json) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {v}"))
}

async fn paths_and_values(acme: &Client, orders: &[i64]) {
    assert_eq!(
        acme.ok("Customers('c1')/Name").await["value"],
        "Ada Lovelace"
    );
    let r = acme.get("Customers('c1')/Name/$value").await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap().starts_with("text/plain"));
    assert_eq!(r.text(), "Ada Lovelace");
    assert_eq!(
        acme.ok("Customers('c1')/Address/City").await["value"],
        "London"
    );
    assert_eq!(acme.get("Customers('c1')/Orders/$count").await.text(), "2");
    assert_eq!(acme.get("Customers/$count").await.text(), "5");
    let order = orders[0];
    assert_eq!(
        acme.ok(&format!("Orders({order})/Customer")).await["Id"],
        "c1"
    );
    let refs = acme.ok("Customers('c1')/Orders/$ref").await;
    let id = refs["value"][0]["@odata.id"].as_str().unwrap();
    assert!(id.contains("Orders("), "{id}");
    assert_eq!(
        acme.ok(&format!("Orders({order})/Lines(OrderId={order},LineNo=2)"))
            .await["Product"],
        "Ink"
    );
    assert_eq!(
        acme.ok("Customers('c1')/Tags").await["value"],
        json!(["math", "poetry"])
    );
    assert_eq!(
        acme.ok("Customers('c1')/Tags?$filter=$it eq 'poetry'")
            .await["value"],
        json!(["poetry"])
    );
}

async fn search_and_compute(acme: &Client) {
    assert_eq!(acme.column("Customers?$search=Turing", "Id").await, ["c3"]);
    assert_eq!(
        acme.column("Customers?$search=Grace OR Edsger&$orderby=Id", "Id")
            .await,
        ["c2", "c4"]
    );
    let doc = acme
        .ok("Orders?$compute=Total mul 2 as Double&$filter=Double gt 100&$select=CustomerId,Double")
        .await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(doc["value"][0]["CustomerId"], "c2");
    assert_eq!(number(&doc["value"][0]["Double"]), 198.0);
    let ids = acme
        .column(
            "Customers?$filter=startswith(Name,'A') and year(Joined) eq 2024&$select=Id",
            "Id",
        )
        .await;
    assert_eq!(ids, ["c1"]);
}

async fn paging(acme: &Client) {
    let (items, pages) = acme.all("Customers?$orderby=Id&$select=Id").await;
    let ids: Vec<_> = items.iter().map(|i| i["Id"].clone()).collect();
    assert_eq!(ids, ["c1", "c2", "c3", "c4", "c5"]);
    assert_eq!(pages, 3, "the paging provider serves two per page");

    let (items, pages) = acme.all("Customers?$orderby=Id&$top=3&$skip=1").await;
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["Id"], "c2");
    assert_eq!(pages, 2);

    let r = acme
        .send(
            "GET",
            "Customers?$orderby=Id",
            &[("Prefer", "odata.maxpagesize=1")],
            "",
        )
        .await;
    assert_eq!(r.status, 200);
    let doc = r.json();
    assert_eq!(doc["value"].as_array().unwrap().len(), 1);
    assert!(doc["@odata.nextLink"].is_string());
}

async fn aggregation(acme: &Client) {
    let doc = acme
        .ok("Orders?$apply=groupby((Status),aggregate(Total with sum as Revenue,$count as N))&$orderby=Status")
        .await;
    let rows = doc["value"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{doc}");
    assert_eq!(rows[0]["Status"], "Open");
    assert_eq!(number(&rows[0]["Revenue"]), 176.75);
    assert_eq!(rows[0]["N"], 4);
    assert_eq!(rows[1]["Status"], "Shipped");
    assert_eq!(number(&rows[1]["Revenue"]), 13.5);

    // Aggregated results are paged like entities.
    let (rows, pages) = acme
        .all("Orders?$apply=groupby((Customer/Address/City),aggregate($count as N))&$orderby=Customer/Address/City")
        .await;
    assert_eq!(pages, 2);
    let cities: Vec<_> = rows
        .iter()
        .map(|r| (r["Customer"]["Address"]["City"].clone(), r["N"].clone()))
        .collect();
    assert_eq!(
        cities,
        [
            (json!("Austin"), json!(1)),
            (json!("London"), json!(3)),
            (json!("New York"), json!(2)),
        ]
    );

    let doc = acme
        .ok("Orders?$apply=topcount(2,Total)&$orderby=Total desc")
        .await;
    let totals: Vec<_> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| number(&o["Total"]))
        .collect();
    assert_eq!(totals, [99.0, 42.0]);

    let doc = acme
        .ok("Customers?$apply=filter(Address/City eq 'London')/aggregate($count as N)")
        .await;
    assert_eq!(doc["value"][0]["N"], 2);
}

async fn concurrency(acme: &Client) {
    let r = acme.get("Customers('c3')").await;
    let etag = r.header("etag").unwrap().to_owned();
    assert_eq!(etag, r.json()["@odata.etag"]);

    let r = acme
        .write(
            "PATCH",
            "Customers('c3')",
            &[("If-Match", &etag)],
            json!({"Name": "A. M. Turing"}),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    let r = acme
        .write(
            "PATCH",
            "Customers('c3')",
            &[("If-Match", &etag)],
            json!({"Name": "Lost update"}),
        )
        .await;
    assert_eq!(r.status, 412, "the ETag moved on");
    let r = acme.get("Customers('c3')").await;
    let doc = r.json();
    assert_eq!(doc["Name"], "A. M. Turing");
    assert_eq!(doc["Version"], 2);
    let etag = r.header("etag").unwrap().to_owned();

    // PUT replaces: what the body leaves out is reset.
    let r = acme
        .write(
            "PUT",
            "Customers('c3')",
            &[("If-Match", &etag), ("Prefer", "return=representation")],
            json!({"Name": "Alan Turing"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let doc = r.json();
    assert_eq!(doc["Address"]["City"], Json::Null);
    assert_eq!(doc["Version"], 3);
    let r = acme
        .write(
            "PATCH",
            "Customers('c3')",
            &[],
            json!({"Address": {"City": "London"}}),
        )
        .await;
    assert_eq!(r.status, 204);

    // Upsert: PATCH on a missing key creates, unless If-Match or If-None-Match says otherwise.
    let r = acme
        .write(
            "PATCH",
            "Customers('c6')",
            &[],
            json!({"Name": "Ken Thompson"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = acme
        .write(
            "PATCH",
            "Customers('c7')",
            &[("If-Match", "*")],
            json!({"Name": "Nobody"}),
        )
        .await;
    assert_eq!(r.status, 412);
    let r = acme
        .write(
            "PUT",
            "Customers('c6')",
            &[("If-None-Match", "*")],
            json!({"Name": "Twice"}),
        )
        .await;
    assert_eq!(r.status, 412);

    let r = acme
        .send("DELETE", "Customers('c6')", &[("If-Match", r#"W/"9""#)], "")
        .await;
    assert_eq!(r.status, 412);
    let r = acme.send("DELETE", "Customers('c6')", &[], "").await;
    assert_eq!(r.status, 204);
    assert_eq!(acme.status("Customers('c6')").await, 404);
}

async fn links(acme: &Client) {
    let r = acme
        .write(
            "POST",
            "Customers('c3')/Categories/$ref",
            &[],
            json!({"@odata.id": "Categories('B2B')"}),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    let r = acme
        .write(
            "POST",
            "Customers('c3')/Categories/$ref",
            &[],
            json!({"@odata.id": "Categories('NEW')"}),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(
        acme.column("Customers('c3')/Categories?$orderby=Code", "Code")
            .await,
        ["B2B", "NEW"]
    );
    let r = acme
        .send(
            "DELETE",
            "Customers('c3')/Categories/$ref?$id=Categories('NEW')",
            &[],
            "",
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(
        acme.column("Customers('c3')/Categories", "Code").await,
        ["B2B"]
    );
    assert_eq!(
        acme.ok("Categories('B2B')?$expand=*").await["Name"],
        "Business"
    );

    // Capabilities: categories are fixed reference data, though customers link to them.
    let r = acme
        .write(
            "PATCH",
            "Categories('B2B')",
            &[],
            json!({"Name": "Renamed"}),
        )
        .await;
    assert_eq!(r.status, 405, "refused by UpdateRestrictions: {}", r.text());
    assert_eq!(acme.ok("Categories('B2B')").await["Name"], "Business");
    let r = acme
        .write("POST", "Categories", &[], json!({"Code": "X", "Name": "X"}))
        .await;
    assert_eq!(r.status, 405, "refused by InsertRestrictions: {}", r.text());
    assert_eq!(acme.status("Categories('X')").await, 404);
}

const BOUNDARY: &str = "batch_e2e";

fn batch_get(url: &str) -> String {
    format!(
        "--{BOUNDARY}\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nGET {url} HTTP/1.1\r\nAccept: application/json\r\n\r\n\r\n"
    )
}

fn batch_change(id: u32, method: &str, url: &str, body: Json) -> String {
    format!(
        "--cs\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\nContent-ID: {id}\r\n\r\n{method} {url} HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{body}\r\n"
    )
}

async fn batches(acme: &Client) {
    let body = format!(
        "{}--{BOUNDARY}\r\nContent-Type: multipart/mixed; boundary=cs\r\n\r\n{}{}--cs--\r\n{}--{BOUNDARY}--\r\n",
        batch_get("Customers/$count"),
        batch_change(
            1,
            "POST",
            "Customers",
            json!({"Id": "c7", "Name": "Batched"})
        ),
        batch_change(
            2,
            "POST",
            "$1/Orders",
            json!({"Status": "Open", "Total": 7})
        ),
        batch_get("Customers('c7')/Orders/$count"),
    );
    let r = acme
        .send(
            "POST",
            "$batch",
            &[(
                "Content-Type",
                &format!("multipart/mixed; boundary={BOUNDARY}"),
            )],
            &body,
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(
        r.header("content-type")
            .unwrap()
            .starts_with("multipart/mixed")
    );
    let text = r.text();
    assert_eq!(text.matches("HTTP/1.1 201 Created").count(), 2, "{text}");
    assert!(
        text.contains("\r\n\r\n5"),
        "count before the change set: {text}"
    );
    assert!(text.ends_with("--\r\n") || text.ends_with("--"), "{text}");
    assert_eq!(acme.get("Customers('c7')/Orders/$count").await.text(), "1");

    // JSON format: an atomicity group that fails leaves nothing behind.
    let body = json!({"requests": [
        {"id": "a", "atomicityGroup": "g", "method": "POST", "url": "Customers",
         "headers": {"content-type": "application/json"},
         "body": {"Id": "c8", "Name": "Rolled back"}},
        {"id": "b", "atomicityGroup": "g", "method": "POST", "url": "$a/Orders",
         "headers": {"content-type": "application/json"},
         "body": {"Status": "Open", "Total": 1, "Lines": [{"LineNo": 1, "Product": "X", "Qty": -1}]}},
        {"id": "c", "method": "GET", "url": "Customers('c7')?$select=Name", "dependsOn": []}
    ]});
    let r = acme
        .write(
            "POST",
            "$batch",
            &[("Prefer", "odata.continue-on-error")],
            body,
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let doc = r.json();
    let statuses: BTreeMap<String, i64> = doc["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap_or_default().to_owned(),
                r["status"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(statuses.get("c"), Some(&200), "{doc}");
    assert!(
        statuses.values().any(|s| *s == 400),
        "the rule refused the line: {doc}"
    );
    assert_eq!(
        acme.status("Customers('c8')").await,
        404,
        "the group rolled back"
    );
}

async fn operations(acme: &Client, orders: &[i64]) {
    let order = orders[0];
    let url = format!("Orders({order})");
    let r = acme.get(&url).await;
    let etag = r.header("etag").unwrap().to_owned();
    assert_eq!(r.json()["Status"], "Open");

    let cancel = format!("{url}/Shop.Cancel");
    let r = acme
        .send("POST", &cancel, &[("If-Match", r#"W/"42""#)], "")
        .await;
    assert_eq!(r.status, 412, "{}", r.text());
    let r = acme.send("POST", &cancel, &[("If-Match", &etag)], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["value"], true);
    let doc = acme.ok(&url).await;
    assert_eq!(doc["Status"], "Cancelled");
    assert_eq!(doc["Version"], 2, "the handler advanced the ETag");
    let r = acme.send("POST", &cancel, &[("If-Match", &etag)], "").await;
    assert_eq!(r.status, 412, "the old ETag is spent");
    assert_eq!(
        acme.status(&format!("{url}/Shop.Cancel")).await,
        405,
        "actions need POST"
    );

    assert_eq!(acme.ok("Greet(name='Ada')").await["value"], "Hello, Ada");
    assert_eq!(
        acme.ok("Greet(name=@n)?@n='Bob'").await["value"],
        "Hello, Bob"
    );
    assert_eq!(acme.ok("Abs(x=-4)").await["value"], 4);
    assert_eq!(
        acme.status("Greet()").await,
        404,
        "no overload without parameters"
    );
    assert_eq!(acme.status("Orders(999999)/Shop.Cancel").await, 405);
    assert_eq!(
        acme.send("POST", "Orders(999999)/Shop.Cancel", &[], "")
            .await
            .status,
        404
    );
}

async fn tenants(acme: &Client, globex: &Client) {
    assert!(globex.column("Customers", "Id").await.is_empty());
    assert_eq!(globex.status("Customers('c1')").await, 404);
    assert_eq!(globex.get("Orders/$count").await.text(), "0");
    assert_eq!(
        globex.all("Categories").await.0.len(),
        3,
        "untenanted types are shared"
    );

    let r = globex
        .write(
            "POST",
            "Customers",
            &[],
            json!({"Id": "c1", "Name": "Globex One", "Tags": ["g"],
            "Categories@odata.bind": ["Categories('B2B')"]}),
        )
        .await;
    assert_eq!(r.status, 201, "keys are unique per tenant: {}", r.text());
    assert_eq!(globex.ok("Customers('c1')").await["Name"], "Globex One");
    assert_eq!(acme.ok("Customers('c1')").await["Name"], "Ada Lovelace");
    assert_eq!(
        acme.ok("Customers('c1')").await["Tags"],
        json!(["math", "poetry"])
    );
    assert_eq!(globex.ok("Customers('c1')").await["Tags"], json!(["g"]));
    assert_eq!(
        globex.column("Customers('c1')/Categories", "Code").await,
        ["B2B"]
    );
    assert_eq!(
        acme.column("Customers('c1')/Categories", "Code").await,
        ["VIP"]
    );

    assert_eq!(
        globex
            .send("DELETE", "Customers('c2')", &[], "")
            .await
            .status,
        404
    );
    assert_eq!(acme.status("Customers('c2')").await, 200);
    let doc = globex.ok("Customers?$apply=aggregate($count as N)").await;
    assert_eq!(doc["value"][0]["N"], 1);
    let r = globex
        .write(
            "POST",
            "Customers('c1')/Orders",
            &[],
            json!({"Status": "Open", "Total": 3}),
        )
        .await;
    assert_eq!(r.status, 201);
    let order = r.json()["Id"].as_i64().unwrap();
    assert_eq!(acme.status(&format!("Orders({order})")).await, 404);
    assert_eq!(
        acme.send("POST", &format!("Orders({order})/Shop.Cancel"), &[], "")
            .await
            .status,
        404
    );
}

async fn visibility(acme: &Client, staff: &Client) {
    let doc = acme.ok("Customers('c1')").await;
    assert!(doc.get("Email").is_none(), "{doc}");
    assert_eq!(
        staff.ok("Customers('c1')").await["Email"],
        "ada@example.org"
    );
    assert_ne!(acme.status("Customers?$filter=Email eq 'x'").await, 200);
    assert_ne!(acme.status("Customers?$select=Email").await, 200);
    assert_eq!(staff.status("Customers?$select=Email").await, 200);
    assert!(
        !acme
            .get("$metadata")
            .await
            .text()
            .contains(r#"Name="Email""#)
    );
    assert!(
        staff
            .get("$metadata")
            .await
            .text()
            .contains(r#"Name="Email""#)
    );
}

/// `(key, removal reason)` of each entry of a delta response.
fn delta_entries(doc: &Json) -> Vec<(String, Option<String>)> {
    let mut out: Vec<_> = doc["value"]
        .as_array()
        .unwrap_or_else(|| panic!("{doc}"))
        .iter()
        .map(|row| {
            let reason = row["reason"]
                .as_str()
                .or_else(|| row["@removed"]["reason"].as_str());
            match reason {
                Some(reason) => {
                    let id = row["id"].as_str().or_else(|| row["@id"].as_str()).unwrap();
                    let key = id.rsplit_once("('").unwrap().1.trim_end_matches("')");
                    (key.to_owned(), Some(reason.to_owned()))
                }
                None => (row["Id"].as_str().unwrap().to_owned(), None),
            }
        })
        .collect();
    out.sort();
    out
}

async fn change_tracking(acme: &Client, globex: &Client) {
    // The first read is paged too: the delta link comes with the last page.
    let r = acme
        .send(
            "GET",
            "Customers?$select=Id,Name",
            &[("Prefer", "odata.track-changes")],
            "",
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(
        r.header("preference-applied")
            .unwrap()
            .contains("odata.track-changes")
    );
    let mut doc = r.json();
    let mut seen = doc["value"].as_array().unwrap().len();
    while let Some(next) = doc["@odata.nextLink"].as_str().map(str::to_owned) {
        assert!(doc.get("@odata.deltaLink").is_none());
        doc = acme.ok(&next).await;
        seen += doc["value"].as_array().unwrap().len();
    }
    assert_eq!(seen, 6, "c1 to c5 and the batched c7");
    let link = doc["@odata.deltaLink"].as_str().unwrap().to_owned();
    assert!(link.contains("$deltatoken="), "{link}");

    assert_eq!(
        acme.write(
            "PATCH",
            "Customers('c4')",
            &[],
            json!({"Name": "E. W. Dijkstra"})
        )
        .await
        .status,
        204
    );
    assert_eq!(
        acme.send("DELETE", "Customers('c5')", &[], "").await.status,
        204
    );
    assert_eq!(
        acme.write(
            "POST",
            "Customers",
            &[],
            json!({"Id": "c9", "Name": "Donald Knuth"})
        )
        .await
        .status,
        201
    );
    assert_eq!(
        globex
            .write(
                "PATCH",
                "Customers('c1')",
                &[],
                json!({"Name": "Globex Prime"})
            )
            .await
            .status,
        204
    );

    let (entries, next) = follow_delta(acme, &link).await;
    assert_eq!(
        entries,
        [
            ("c4".to_owned(), None),
            ("c5".to_owned(), Some("deleted".to_owned())),
            ("c9".to_owned(), None),
        ],
        "only this tenant's changes, not globex's c1"
    );
    let (entries, _) = follow_delta(acme, &next).await;
    assert!(entries.is_empty(), "{entries:?}");
}

/// The entries of a delta link across its pages, and the link that follows them.
async fn follow_delta(client: &Client, link: &str) -> (Vec<(String, Option<String>)>, String) {
    let mut entries = Vec::new();
    let mut doc = client.ok(link).await;
    loop {
        assert!(
            doc["@odata.context"].as_str().unwrap().ends_with("/$delta"),
            "{doc}"
        );
        entries.extend(delta_entries(&doc));
        match doc["@odata.nextLink"].as_str() {
            Some(next) => doc = client.ok(next).await,
            None => break,
        }
    }
    entries.sort();
    let next = doc["@odata.deltaLink"].as_str().unwrap().to_owned();
    (entries, next)
}

async fn errors(acme: &Client) {
    let expect = |r: Reply, status: u16| {
        assert_eq!(r.status, status, "{}", r.text());
        let doc = r.json();
        assert!(doc["error"]["code"].is_string(), "{doc}");
        assert!(doc["error"]["message"].is_string(), "{doc}");
    };
    expect(
        acme.send(
            "POST",
            "Customers",
            &[("Content-Type", "application/json")],
            "{not json",
        )
        .await,
        400,
    );
    expect(
        acme.write(
            "POST",
            "Customers",
            &[],
            json!({"Id": "e1", "Name": "x", "Nope": 1}),
        )
        .await,
        400,
    );
    expect(
        acme.write("POST", "Customers", &[], json!({"Id": "e1", "Name": 5}))
            .await,
        400,
    );
    expect(
        acme.write("POST", "Customers", &[], json!({"Id": "e1"}))
            .await,
        400,
    );
    expect(
        acme.write(
            "POST",
            "Customers",
            &[],
            json!({"Id": "e1", "Name": "x".repeat(41)}),
        )
        .await,
        400,
    );
    expect(
        acme.write(
            "POST",
            "Customers",
            &[],
            json!({"Id": "c1", "Name": "Duplicate"}),
        )
        .await,
        409,
    );
    expect(
        acme.write(
            "POST",
            "Orders",
            &[],
            json!({"CustomerId": "nobody", "Status": "Open", "Total": 1}),
        )
        .await,
        409,
    );
    expect(
        acme.write(
            "POST",
            "Orders",
            &[],
            json!({"CustomerId": "c1", "Status": "Lost", "Total": 1}),
        )
        .await,
        400,
    );
    expect(acme.get("Customers?$filter=Name eq").await, 400);
    expect(acme.get("Customers?$orderby=Nope").await, 400);
    expect(acme.get("Customers?$top=-1").await, 400);
    expect(acme.get("Customers?$frobnicate=1").await, 400);
    expect(acme.get("Customers('c1')/Nope").await, 404);
    expect(acme.get("Nowhere").await, 404);
    expect(acme.get("Customers('zzz')").await, 404);
    expect(
        acme.send(
            "POST",
            "Customers('c1')",
            &[("Content-Type", "application/json")],
            "{}",
        )
        .await,
        405,
    );
    expect(
        acme.send("GET", "Customers", &[("Accept", "application/xml")], "")
            .await,
        406,
    );
    let r = acme
        .send(
            "GET",
            "Customers?$filter=Name eq 'x'' or 1 eq 1 --'",
            &[],
            "",
        )
        .await;
    assert_eq!(r.status, 200);
    assert!(
        r.json()["value"].as_array().unwrap().is_empty(),
        "quotes are data"
    );
    assert_eq!(
        acme.get("Customers/$count").await.text(),
        "6",
        "nothing broke"
    );
}

async fn parallel_clients(acme: &Client, globex: &Client) {
    let mut tasks = Vec::new();
    for i in 0..24 {
        let client = if i % 2 == 0 {
            acme.clone()
        } else {
            globex.clone()
        };
        tasks.push(tokio::spawn(async move {
            let doc = client
                .ok("Customers('c1')?$select=Name&$expand=Orders($select=Id)")
                .await;
            (
                client.tenant.clone().unwrap(),
                doc["Name"].as_str().unwrap().to_owned(),
            )
        }));
    }
    for task in tasks {
        let (tenant, name) = task.await.unwrap();
        let expected = if tenant == "acme" {
            "Ada Lovelace"
        } else {
            "Globex Prime"
        };
        assert_eq!(name, expected, "{tenant}");
    }
}

#[tokio::test]
async fn shop_on_sqlite() {
    shop(common::sqlite_pool(SCHEMA).await.into()).await;
}

#[tokio::test]
async fn shop_on_postgres() {
    if !common::postgres_can_run() {
        return;
    }
    let server = common::PostgresServer::start().await;
    shop(server.pool(SCHEMA).await.into()).await;
}
