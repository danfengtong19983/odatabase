//! A small service on in-memory SQLite. Run it, then try the URLs in the README.
//!
//! ```bash
//! cargo run -p odatabase --example quickstart
//! ```

use odatabase::{Nav, ODataComplex, ODataEntity, Service};

#[derive(ODataComplex)]
pub struct Address {
    pub street: Option<String>,
    pub city: Option<String>,
}

#[derive(ODataEntity)]
#[odata(table = "customer")]
pub struct Customer {
    #[odata(key)]
    pub id: String,
    #[odata(etag)]
    pub version: i64,
    #[odata(max_length = 100)]
    pub name: String,
    #[odata(embedded(prefix = "addr_"))]
    pub address: Address,
    #[odata(has_many(target = Order, on(id = "customer_id"), partner = "Customer"))]
    pub orders: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "orders")]
pub struct Order {
    #[odata(key, computed)]
    pub id: i64,
    pub customer_id: String,
    pub total: f64,
    #[odata(belongs_to(target = Customer, on(customer_id = "id"), required, partner = "Orders"))]
    pub customer: Nav,
}

const SCHEMA: &str = "
CREATE TABLE customer (
  id TEXT PRIMARY KEY, version BIGINT NOT NULL, name TEXT NOT NULL,
  addr_street TEXT, addr_city TEXT
);
CREATE TABLE orders (
  id INTEGER PRIMARY KEY, customer_id TEXT NOT NULL REFERENCES customer (id), total REAL NOT NULL
);
INSERT INTO customer VALUES ('c1', 1, 'Ada', 'Main 1', 'London'), ('c2', 1, 'Alan', NULL, 'Wilmslow');
INSERT INTO orders (customer_id, total) VALUES ('c1', 20.5), ('c1', 7), ('c2', 99);
";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .in_memory(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;

    let service = Service::builder("Shop").entity::<Customer>().build(pool)?;
    let app = axum::Router::new().nest("/odata", odatabase::router(service));
    let address = std::env::var("ODATA_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_owned());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("serving http://{address}/odata");
    axum::serve(listener, app).await?;
    Ok(())
}
