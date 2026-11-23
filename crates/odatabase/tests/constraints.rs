//! Database constraint violations: their generic statuses, and errors registered per constraint
//! with `ServiceBuilder::constraint_error`.

mod common;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::App;
use odatabase::sql::sea_query::{Alias, Query};
use odatabase::{
    ChangeContext, ChangeHandler, Database, EntityPayload, ODataEntity, ODataError, PropertyValue,
    Service,
};
use serde_json::{Value as Json, json};

const SCHEMA: &str = "
CREATE TABLE account (
  id INTEGER PRIMARY KEY,
  email TEXT NOT NULL,
  balance INTEGER NOT NULL,
  CONSTRAINT account_email_uk UNIQUE (email),
  CONSTRAINT account_balance_ck CHECK (balance >= 0)
);
INSERT INTO account (id, email, balance) VALUES (1, 'a@example.com', 10);
";

#[derive(ODataEntity)]
#[odata(table = "account", rename_all = "snake_case")]
pub struct Account {
    #[odata(key)]
    pub id: i32,
    pub email: String,
    pub balance: i32,
}

/// Writes a second row with the email of the one created, inside the request, so that the
/// handler sees the constraint of its own failed statement.
struct Shadow;

#[async_trait]
impl ChangeHandler for Shadow {
    async fn before_create(
        &self,
        ctx: &ChangeContext<'_>,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        let is_77 = |(k, v): &(String, PropertyValue)| {
            k == "id" && matches!(v, PropertyValue::Value(v) if v.as_i64() == Some(77))
        };
        if entity.values.iter().any(is_77) {
            let mut insert = Query::insert();
            insert
                .into_table(Alias::new("account"))
                .columns([Alias::new("id"), Alias::new("email"), Alias::new("balance")])
                .values_panic([78.into(), "a@example.com".into(), 0.into()]);
            let failed = ctx
                .session
                .execute(&insert, &[])
                .await
                .map_err(ODataError::from);
            let constraint = failed.err().and_then(|e| e.constraint);
            return Err(ODataError::new(422, format!("handler saw {constraint:?}")));
        }
        Ok(())
    }
}

fn service(db: Database) -> Service {
    Service::builder("C")
        .entity::<Account>()
        .change_handler(Shadow)
        .constraint_error(
            "account_email_uk",
            ODataError::new(409, "an account with this email exists").with_code("email_taken"),
        )
        .build(db)
        .unwrap()
}

async fn post(app: &App, body: Json) -> common::Response {
    app.request(
        Method::POST,
        "Accounts",
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await
}

/// Constraints without a registered error answer the generic status, on both databases.
async fn generic(app: App, _postgres: bool) {
    let r = post(
        &app,
        json!({"id": 2, "email": "b@example.com", "balance": -1}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "check: {}", r.text());
    assert_eq!(
        r.json()["error"]["message"],
        "a value breaks a rule of the database"
    );

    let r = post(
        &app,
        json!({"id": 1, "email": "c@example.com", "balance": 0}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "primary key: {}", r.text());
    assert_eq!(r.json()["error"]["code"], "409");
}

/// The registered error replaces the generic one where the database names the constraint.
async fn mapped(app: App, postgres: bool) {
    let r = post(
        &app,
        json!({"id": 3, "email": "a@example.com", "balance": 0}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.text());
    let error = &r.json()["error"];
    if postgres {
        assert_eq!(error["code"], "email_taken");
        assert_eq!(error["message"], "an account with this email exists");
    } else {
        assert_eq!(error["code"], "409", "SQLite names no constraint");
        assert_eq!(error["message"], "the entity already exists");
    }

    let r = app
        .request(
            Method::PATCH,
            "Accounts(1)",
            &[("content-type", "application/json")],
            &json!({"email": "a@example.com", "balance": 1}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "own email: {}", r.text());

    let batch = json!({"requests": [
        {"id": "a", "atomicityGroup": "g", "method": "post", "url": "Accounts",
         "headers": {"content-type": "application/json"},
         "body": {"id": 4, "email": "d@example.com", "balance": 0}},
        {"id": "b", "atomicityGroup": "g", "method": "post", "url": "Accounts",
         "headers": {"content-type": "application/json"},
         "body": {"id": 5, "email": "d@example.com", "balance": 0}}
    ]});
    let r = app
        .request(
            Method::POST,
            "$batch",
            &[("content-type", "application/json")],
            &batch.to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let doc = r.json();
    assert_eq!(doc["responses"].as_array().unwrap().len(), 1, "{doc}");
    assert_eq!(doc["responses"][0]["status"], 409);
    if postgres {
        assert_eq!(
            doc["responses"][0]["body"]["error"]["code"], "email_taken",
            "{doc}"
        );
    }
    assert_eq!(app.status("Accounts(4)").await, 404, "rolled back");
}

async fn handler_sees_the_constraint(app: App, postgres: bool) {
    let r = post(
        &app,
        json!({"id": 77, "email": "x@example.com", "balance": 0}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.text());
    let expected = if postgres {
        "handler saw Some(\"account_email_uk\")"
    } else {
        "handler saw None"
    };
    assert_eq!(r.json()["error"]["message"], expected);
}

macro_rules! constraint_scenarios {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let pool = common::sqlite_pool(super::SCHEMA).await;
                super::$name(App::new(super::service(pool.into())), false).await;
            }
        )*

        #[tokio::test]
        async fn on_postgres() {
            if !common::postgres_can_run() {
                return;
            }
            let server = common::PostgresServer::start().await;
            $(
                let pool = server.pool(super::SCHEMA).await;
                eprintln!("postgres: {}", stringify!($name));
                super::$name(App::new(super::service(pool.into())), true).await;
            )*
        }
    };
}

mod scenario {
    use super::*;
    constraint_scenarios!(generic, mapped, handler_sees_the_constraint);
}
