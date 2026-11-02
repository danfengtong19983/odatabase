//! Serves the test model for third-party OData clients. See `conformance/README.md`.

#[path = "../tests/common/mod.rs"]
mod common;

use async_trait::async_trait;
use odatabase::edm::Value;
use odatabase::{
    Database, ODataEntity, ODataError, OffsetPaging, OperationContext, OperationHandler,
    OperationResult,
};

/// The entity clients track changes on.
#[derive(ODataEntity)]
#[odata(table = "note", track_changes)]
pub struct Note {
    #[odata(key)]
    pub id: i32,
    pub title: String,
}

const NOTES: &str = "
CREATE TABLE note (id INTEGER PRIMARY KEY, title TEXT NOT NULL);
INSERT INTO note (id, title) VALUES (2, 'second'), (3, 'third');
";

struct Add;

#[async_trait]
impl OperationHandler for Add {
    async fn call(&self, ctx: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        let a = ctx.value("a").and_then(Value::as_i64).unwrap_or(0);
        let b = ctx.value("b").and_then(Value::as_i64).unwrap_or(0);
        Ok(OperationResult::Value(Value::Int32((a + b) as i32)))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::sqlite_pool(&format!("{}{NOTES}", common::SCHEMA)).await;
    let database = Database::from(pool);
    database.create_change_log().await?;
    let service = common::builder()
        .entity::<Note>()
        .paging(OffsetPaging {
            page_size: Some(100),
        })
        .function("Add", |f| {
            f.param::<i32>("a")
                .param::<i32>("b")
                .returns::<i32>()
                .handler(Add)
        })
        .action("Sum", |a| {
            a.param::<i32>("a")
                .param::<i32>("b")
                .returns::<i32>()
                .handler(Add)
        })
        .build(database)?;
    let app = odatabase::mount("/odata", service);
    let address = std::env::var("ODATA_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_owned());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("serving http://{address}/odata");
    axum::serve(listener, app).await?;
    Ok(())
}
