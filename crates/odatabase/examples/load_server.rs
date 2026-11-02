//! Serves the load test schema from `loadtest/seed.sql`. See `loadtest/README.md`.

use odatabase::{Database, Nav, ODataEntity, OffsetPaging, PostgresOptions, Service};

#[derive(ODataEntity)]
#[odata(table = "customer")]
pub struct Customer {
    #[odata(key)]
    pub id: i64,
    #[odata(etag)]
    pub version: i64,
    pub name: String,
    pub country: String,
    pub city: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[odata(has_many(target = Order, on(id = "customer_id"), partner = "Customer"))]
    pub orders: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "orders")]
pub struct Order {
    #[odata(key)]
    pub id: i64,
    pub customer_id: i64,
    pub status: String,
    pub total: rust_decimal::Decimal,
    pub placed_at: chrono::DateTime<chrono::Utc>,
    #[odata(belongs_to(target = Customer, on(customer_id = "id"), required, partner = "Orders"))]
    pub customer: Nav,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")?;
    let connections = std::env::var("POOL_SIZE").map_or(Ok(16), |v| v.parse())?;
    let database = Database::connect_postgres(
        &url,
        PostgresOptions {
            max_connections: connections,
            ..PostgresOptions::default()
        },
    )
    .await?;
    let page_size = match std::env::var("PAGE_SIZE").as_deref() {
        Ok("none") => None,
        Ok(n) => Some(n.parse()?),
        Err(_) => Some(1000),
    };
    let service = Service::builder("Load")
        .entity::<Customer>()
        .paging(OffsetPaging { page_size })
        .build(database)?;
    let app = odatabase::mount("/odata", service);
    let address = std::env::var("ODATA_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_owned());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("serving http://{address}/odata");
    axum::serve(listener, app).await?;
    Ok(())
}
