//! Change tracking on a type with a query extension: what delta responses say about rows the
//! extension hides (`QueryExtension::tracks_changes` and `hidden_changes`).

mod common;

use axum::Extension;
use axum::http::{Method, StatusCode};
use common::App;
use odatabase::sql::sea_query::{Expr, ExprTrait};
use odatabase::{
    Claims, Database, HiddenChanges, ODataEntity, QueryError, QueryExtension, Service, TableRef,
};
use serde_json::{Value as Json, json};

const SCHEMA: &str = "
CREATE TABLE note (id TEXT PRIMARY KEY, owner TEXT NOT NULL, title TEXT NOT NULL);
INSERT INTO note (id, owner, title) VALUES
  ('n1', 'ann', 'A'), ('n2', 'bob', 'A'), ('n3', 'ann', 'A'), ('n4', 'ann', 'A');
";

#[derive(ODataEntity)]
#[odata(
    table = "note",
    rename_all = "snake_case",
    query_extension,
    track_changes
)]
pub struct Note {
    #[odata(key)]
    pub id: String,
    pub owner: String,
    pub title: String,
}

/// Each caller reads the notes they own, named by their groups; the group `all` reads every note.
struct Owners {
    tracks: bool,
    hidden: HiddenChanges,
}

impl QueryExtension for Owners {
    fn condition(&self, table: &TableRef, claims: &Claims) -> Result<Option<Expr>, QueryError> {
        if claims.groups.iter().any(|g| g == "all") {
            return Ok(None);
        }
        Ok(Some(table.col("owner").is_in(claims.groups.clone())))
    }

    fn tracks_changes(&self) -> bool {
        self.tracks
    }

    fn hidden_changes(&self, _: &Claims) -> HiddenChanges {
        self.hidden
    }
}

fn service(db: Database, hidden: HiddenChanges) -> Result<Service, String> {
    Service::builder("T")
        .entity::<Note>()
        .query_extension::<Note>(Owners {
            tracks: true,
            hidden,
        })
        .build(db)
        .map_err(|e| e.to_string())
}

struct Callers {
    admin: App,
    ann: App,
}

impl Callers {
    async fn new(db: Database, hidden: HiddenChanges) -> Self {
        db.create_change_log().await.unwrap();
        let app = App::new(service(db, hidden).unwrap());
        let as_group = |g: &str| App {
            router: app
                .router
                .clone()
                .layer(Extension(Claims::default().with_group(g))),
        };
        Self {
            admin: as_group("all"),
            ann: as_group("ann"),
        }
    }

    async fn link(&self, uri: &str) -> String {
        let r = self
            .ann
            .request(Method::GET, uri, &[("prefer", "odata.track-changes")], "")
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        r.json()["@odata.deltaLink"]
            .as_str()
            .unwrap()
            .trim_start_matches("http://test.local/odata/")
            .to_owned()
    }

    async fn write(&self, method: Method, uri: &str, body: Json) {
        let r = self
            .admin
            .request(
                method,
                uri,
                &[("content-type", "application/json")],
                &body.to_string(),
            )
            .await;
        assert!(r.status.is_success(), "{uri}: {}", r.text());
    }

    /// `(id, removal reason)` for each entry of ann's delta page.
    async fn delta(&self, link: &str) -> Vec<(String, Option<String>)> {
        let doc = self.ann.ok(link).await;
        doc["value"]
            .as_array()
            .unwrap_or_else(|| panic!("{doc}"))
            .iter()
            .map(|row| match row["reason"].as_str() {
                Some(reason) => {
                    let id = row["id"].as_str().unwrap().rsplit('/').next().unwrap();
                    (id.to_owned(), Some(reason.to_owned()))
                }
                None => (row["id"].as_str().unwrap().to_owned(), None),
            })
            .collect()
    }

    /// Changes another caller's note, moves one of ann's away from her, changes one of hers so
    /// that it leaves her filter, and deletes another of hers.
    async fn changes(&self) {
        self.write(Method::PATCH, "Notes('n2')", json!({"title": "B"}))
            .await;
        self.write(Method::PATCH, "Notes('n1')", json!({"owner": "bob"}))
            .await;
        self.write(Method::PATCH, "Notes('n3')", json!({"title": "B"}))
            .await;
        self.write(Method::DELETE, "Notes('n4')", Json::Null).await;
    }
}

fn entry(id: &str, reason: Option<&str>) -> (String, Option<String>) {
    (id.to_owned(), reason.map(str::to_owned))
}

/// The default: only rows ann can still read are reported as removed, so no other key leaks.
async fn silent(db: Database) {
    let c = Callers::new(db, HiddenChanges::Silent).await;
    let link = c.link("Notes?$filter=title eq 'A'").await;
    c.changes().await;
    assert_eq!(
        c.delta(&link).await,
        [entry("Notes('n3')", Some("changed"))],
        "n2 and n1 are hidden from ann now, and n4 is gone: none is reported"
    );

    let link = c.link("Notes").await;
    c.write(Method::PATCH, "Notes('n3')", json!({"title": "C"}))
        .await;
    c.write(Method::PATCH, "Notes('n2')", json!({"title": "C"}))
        .await;
    assert_eq!(c.delta(&link).await, [entry("n3", None)]);
}

/// `Removed`: every change ann can't read is reported, revealing its key.
async fn removed(db: Database) {
    let c = Callers::new(db, HiddenChanges::Removed).await;
    let link = c.link("Notes?$filter=title eq 'A'").await;
    c.changes().await;
    assert_eq!(
        c.delta(&link).await,
        [
            entry("Notes('n2')", Some("changed")),
            entry("Notes('n1')", Some("changed")),
            entry("Notes('n3')", Some("changed")),
            entry("Notes('n4')", Some("deleted")),
        ]
    );
}

macro_rules! tracking_scenarios {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let pool = common::sqlite_pool(super::SCHEMA).await;
                super::$name(pool.into()).await;
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
                super::$name(pool.into()).await;
            )*
        }
    };
}

mod scenario {
    use super::*;
    tracking_scenarios!(silent, removed);
}

#[tokio::test]
async fn an_extension_must_allow_tracking() {
    let db: Database = common::sqlite_pool(SCHEMA).await.into();
    let e = Service::builder("T")
        .entity::<Note>()
        .query_extension::<Note>(Owners {
            tracks: false,
            hidden: HiddenChanges::Silent,
        })
        .build(db)
        .err()
        .map(|e| e.to_string());
    assert!(
        e.as_deref()
            .is_some_and(|e| e.contains("doesn't allow tracking changes")),
        "{e:?}"
    );
}
