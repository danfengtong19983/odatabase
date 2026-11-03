mod common;

use axum::Extension;
use axum::http::{Method, StatusCode};
use common::App;
use odatabase::sql::{ChangeKind, Session, record_change};
use odatabase::{Claims, Database, Nav, ODataEntity, Service, ServiceBuilder};
use serde_json::{Value, json};
use std::time::Duration;

const SCHEMA: &str = "
CREATE TABLE person (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE project (id INTEGER PRIMARY KEY, title TEXT NOT NULL);
CREATE TABLE project_member (
  project_id INTEGER NOT NULL REFERENCES project (id),
  person_id INTEGER NOT NULL REFERENCES person (id),
  PRIMARY KEY (project_id, person_id)
);
CREATE TABLE task (
  id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  priority INTEGER,
  budget INTEGER,
  project_id INTEGER REFERENCES project (id)
);
CREATE TABLE task_tag (task_id TEXT NOT NULL REFERENCES task (id), position INTEGER NOT NULL, tag TEXT NOT NULL);
CREATE TABLE label (
  task_id TEXT NOT NULL REFERENCES task (id),
  code INTEGER NOT NULL,
  body TEXT,
  PRIMARY KEY (task_id, code)
);
CREATE TABLE archive (id INTEGER PRIMARY KEY, title TEXT);
INSERT INTO person (id, name) VALUES (1, 'Ann'), (2, 'Bob'), (3, 'Cid');
INSERT INTO project (id, title) VALUES (1, 'Alpha'), (2, 'Beta');
INSERT INTO project_member (project_id, person_id) VALUES (1, 1), (1, 2);
INSERT INTO task (id, title, priority, budget, project_id) VALUES
  ('a', 'First', 9, 100, 1), ('b', 'Second', 5, 200, 1), ('c', 'Third', 1, 300, 2);
INSERT INTO task_tag (task_id, position, tag) VALUES ('a', 1, 'x');
INSERT INTO label (task_id, code, body) VALUES ('a', 1, 'one'), ('a', 2, 'two'), ('b', 1, 'other');
INSERT INTO archive (id, title) VALUES (1, 'old');
";

#[derive(ODataEntity)]
#[odata(table = "person", set = "People")]
pub struct Person {
    #[odata(key)]
    pub id: i32,
    pub name: String,
}

#[derive(ODataEntity)]
#[odata(table = "project", track_changes)]
pub struct Project {
    #[odata(key)]
    pub id: i32,
    pub title: String,
    #[odata(has_many(target = Task, on(id = "project_id")))]
    pub tasks: Nav,
    #[odata(has_many(
        target = Person,
        join_table(
            table = "project_member",
            source(id = "project_id"),
            target(person_id = "id")
        )
    ))]
    pub members: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "task", track_changes)]
pub struct Task {
    #[odata(key)]
    pub id: String,
    pub title: String,
    pub priority: Option<i32>,
    #[odata(visible_for("hr"))]
    pub budget: Option<i32>,
    pub project_id: Option<i32>,
    #[odata(collection(
        table = "task_tag",
        owner(id = "task_id"),
        value_column = "tag",
        order_column = "position"
    ))]
    pub tags: Vec<String>,
    #[odata(belongs_to(target = Project, on(project_id = "id")))]
    pub project: Nav,
    #[odata(has_many(target = Label, on(id = "task_id")))]
    pub labels: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "label", track_changes)]
pub struct Label {
    #[odata(key)]
    pub task_id: String,
    #[odata(key)]
    pub code: i32,
    pub body: Option<String>,
}

#[derive(ODataEntity)]
#[odata(
    table = "archive",
    set = "Archive",
    visible_for("admin"),
    track_changes
)]
pub struct Archived {
    #[odata(key)]
    pub id: i32,
    pub title: Option<String>,
}

fn builder() -> ServiceBuilder {
    Service::builder("Delta")
        .entity::<Person>()
        .entity::<Project>()
        .entity::<Task>()
        .entity::<Label>()
        .entity::<Archived>()
}

struct Harness {
    app: App,
    db: Database,
}

impl Harness {
    async fn new(db: Database) -> Self {
        db.create_change_log().await.unwrap();
        let app = App::new(builder().build(db.clone()).unwrap());
        Self { app, db }
    }

    async fn track(&self, uri: &str) -> Value {
        let r = self.tracked(uri, "odata.track-changes").await;
        assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.text());
        assert_eq!(r.header("preference-applied"), Some("odata.track-changes"));
        r.json()
    }

    async fn tracked(&self, uri: &str, prefer: &str) -> common::Response {
        self.app
            .request(Method::GET, uri, &[("prefer", prefer)], "")
            .await
    }

    /// The delta link of a first read, for use as a relative URI.
    async fn link(&self, uri: &str) -> String {
        relative(&self.track(uri).await["@odata.deltaLink"])
    }

    async fn write(&self, method: Method, uri: &str, body: Value) -> common::Response {
        self.app
            .request(
                method,
                uri,
                &[("content-type", "application/json")],
                &body.to_string(),
            )
            .await
    }

    async fn done(&self, method: Method, uri: &str, body: Value) {
        let r = self.write(method, uri, body).await;
        assert!(r.status.is_success(), "{uri}: {}", r.text());
    }
}

fn relative(link: &Value) -> String {
    link.as_str()
        .unwrap_or_else(|| panic!("no link: {link}"))
        .trim_start_matches("http://test.local/odata/")
        .to_owned()
}

/// Rows of a delta page: `(key field, removal reason)` for each entry.
fn entries(doc: &Value, key: &str) -> Vec<(String, Option<String>)> {
    doc["value"]
        .as_array()
        .unwrap_or_else(|| panic!("{doc}"))
        .iter()
        .map(|row| match row["reason"].as_str() {
            Some(reason) => {
                let id = row["id"].as_str().unwrap();
                (
                    id.rsplit('/').next().unwrap().to_owned(),
                    Some(reason.to_owned()),
                )
            }
            None => (row[key].as_str().unwrap_or_default().to_owned(), None),
        })
        .collect()
}

fn hit(key: &str) -> (String, Option<String>) {
    (key.to_owned(), None)
}

fn gone(key: &str, reason: &str) -> (String, Option<String>) {
    (key.to_owned(), Some(reason.to_owned()))
}

async fn first_read(h: Harness) {
    let doc = h.track("Tasks").await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 3);
    assert!(
        doc["@odata.deltaLink"]
            .as_str()
            .unwrap()
            .contains("$deltatoken=")
    );
    let r = h.tracked("Tasks", "track-changes").await;
    assert_eq!(r.header("preference-applied"), Some("odata.track-changes"));

    let plain = h.app.get("Tasks").await;
    assert!(plain.header("preference-applied").is_none());
    assert!(plain.json()["@odata.deltaLink"].is_null());

    for uri in [
        "People",
        "Tasks?$top=1",
        "Tasks?$skip=1",
        "Tasks?$expand=Labels",
        "Tasks?$apply=filter(Priority gt 1)",
        "Tasks?$filter=Project/Title eq 'Alpha'",
        "Tasks?$filter=Labels/any(l: l/Code eq 1)",
        "Projects(1)/Tasks",
        "Tasks('a')/Labels",
    ] {
        let r = h.tracked(uri, "odata.track-changes").await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
        assert!(r.header("preference-applied").is_none(), "{uri}");
        assert!(r.json()["@odata.deltaLink"].is_null(), "{uri}");
    }
}

async fn changes(h: Harness) {
    let link = h.link("Tasks").await;
    let doc = h.app.ok(&link).await;
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#Tasks/$delta"),
        "{doc}"
    );
    assert_eq!(entries(&doc, "Id"), [], "nothing happened yet");

    h.done(Method::POST, "Tasks", json!({"Id": "d", "Title": "Fourth"}))
        .await;
    h.done(Method::PATCH, "Tasks('a')", json!({"Title": "Renamed"}))
        .await;
    h.done(Method::DELETE, "Labels(TaskId='b',Code=1)", json!({}))
        .await;
    h.done(Method::DELETE, "Tasks('b')", json!({})).await;
    let doc = h.app.ok(&link).await;
    assert_eq!(
        entries(&doc, "Id"),
        [hit("d"), hit("a"), gone("Tasks('b')", "deleted")]
    );
    assert_eq!(doc["value"][1]["Title"], "Renamed", "the current state");
    let removed = &doc["value"][2];
    assert_eq!(
        removed["@odata.context"],
        "http://test.local/odata/$metadata#Tasks/$deletedEntity"
    );
    assert_eq!(removed["id"], "http://test.local/odata/Tasks('b')");
    let next = relative(&doc["@odata.deltaLink"]);
    assert_eq!(
        entries(&h.app.ok(&next).await, "Id"),
        [],
        "a new starting point"
    );
    assert_eq!(
        entries(&h.app.ok(&link).await, "Id").len(),
        3,
        "an old link stays valid"
    );

    h.done(Method::PATCH, "Tasks('a')", json!({"Title": "One"}))
        .await;
    h.done(Method::PATCH, "Tasks('a')", json!({"Title": "Two"}))
        .await;
    let doc = h.app.ok(&next).await;
    assert_eq!(entries(&doc, "Id"), [hit("a")], "two changes, one entry");
    assert_eq!(doc["value"][0]["Title"], "Two");

    let next = relative(&doc["@odata.deltaLink"]);
    h.done(Method::POST, "Tasks", json!({"Id": "e", "Title": "New"}))
        .await;
    h.done(Method::DELETE, "Tasks('e')", json!({})).await;
    h.done(Method::POST, "Tasks", json!({"Id": "e", "Title": "Back"}))
        .await;
    let doc = h.app.ok(&next).await;
    assert_eq!(entries(&doc, "Id"), [hit("e")], "the row exists again");
}

async fn every_write_is_logged(h: Harness) {
    for (method, uri, body) in [
        (Method::PATCH, "Tasks('a')/Title", json!({"value": "T"})),
        (Method::PUT, "Tasks('a')/Tags", json!({"value": ["y"]})),
        (Method::DELETE, "Tasks('a')/Priority", json!({})),
        (Method::DELETE, "Tasks('a')/Tags", json!({})),
        (
            Method::PUT,
            "Tasks('a')/Project/$ref",
            json!({"@odata.id": "Projects(2)"}),
        ),
        (Method::DELETE, "Tasks('a')/Project/$ref", json!({})),
        (
            Method::POST,
            "Projects(1)/Tasks/$ref",
            json!({"@odata.id": "Tasks('a')"}),
        ),
        (
            Method::PATCH,
            "Tasks('a')",
            json!({"Project": {"Id": 2, "Title": "Deep"}}),
        ),
    ] {
        let link = h.link("Tasks").await;
        h.done(method, uri, body).await;
        assert_eq!(entries(&h.app.ok(&link).await, "Id"), [hit("a")], "{uri}");
    }

    let link = h.link("Projects").await;
    h.done(
        Method::PATCH,
        "Tasks('a')",
        json!({"Project": {"Id": 2, "Title": "Deeper"}}),
    )
    .await;
    assert_eq!(
        h.app.ok(&link).await["value"][0]["Title"],
        "Deeper",
        "nested entities are logged under their own set"
    );

    let link = h.link("Tasks").await;
    h.done(
        Method::POST,
        "Projects",
        json!({"Id": 3, "Title": "P", "Tasks": [{"Id": "n", "Title": "Nested"}]}),
    )
    .await;
    assert_eq!(entries(&h.app.ok(&link).await, "Id"), [hit("n")]);

    let link = h.link("Tasks").await;
    h.done(
        Method::PATCH,
        "Projects(1)",
        json!({"Tasks": [{"Id": "a"}]}),
    )
    .await;
    assert_eq!(
        entries(&h.app.ok(&link).await, "Id"),
        [hit("a"), hit("b")],
        "b lost its project when the body left it out"
    );

    let link = h.link("Tasks").await;
    let bad = h
        .write(
            Method::PATCH,
            "Tasks('a')",
            json!({"Id": "z", "Title": "No"}),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        entries(&h.app.ok(&link).await, "Id"),
        [],
        "failures leave no trace"
    );

    let batch = json!({"requests": [
        {"id": "1", "atomicityGroup": "g", "method": "PATCH", "url": "Tasks('a')",
         "headers": {"content-type": "application/json"}, "body": {"Title": "Batched"}},
        {"id": "2", "atomicityGroup": "g", "method": "PATCH", "url": "Tasks('nope')",
         "headers": {"content-type": "application/json", "if-match": "*"}, "body": {"Title": "x"}},
    ]});
    h.write(Method::POST, "$batch", batch).await;
    assert_eq!(
        entries(&h.app.ok(&link).await, "Id"),
        [],
        "a failed change set leaves no trace"
    );
}

async fn defining_query(h: Harness) {
    let link = h.link("Tasks?$filter=Priority gt 3&$select=Title").await;
    assert!(link.contains("$filter="), "{link}");
    h.done(Method::PATCH, "Tasks('a')", json!({"Priority": 1}))
        .await;
    h.done(Method::PATCH, "Tasks('c')", json!({"Priority": 8}))
        .await;
    h.done(Method::PATCH, "Tasks('b')", json!({"Title": "Still in"}))
        .await;
    let doc = h.app.ok(&link).await;
    assert_eq!(
        entries(&doc, "Id"),
        [gone("Tasks('a')", "changed"), hit("c"), hit("b")],
        "rows leave and enter the result"
    );
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#Tasks(Title)/$delta"),
        "{doc}"
    );
    assert!(
        doc["value"][1]["Priority"].is_null(),
        "$select holds: {doc}"
    );
    assert!(
        relative(&doc["@odata.deltaLink"]).contains("$filter="),
        "the next link keeps the query"
    );

    let link = h.link("Labels").await;
    h.done(
        Method::PATCH,
        "Labels(TaskId='a',Code=2)",
        json!({"Body": "new"}),
    )
    .await;
    h.done(Method::DELETE, "Labels(TaskId='a',Code=1)", json!({}))
        .await;
    let doc = h.app.ok(&link).await;
    assert_eq!(doc["value"][0]["Code"], 2, "composite keys: {doc}");
    assert_eq!(
        doc["value"][1]["id"],
        "http://test.local/odata/Labels(TaskId='a',Code=1)"
    );

    let none = h
        .app
        .request(
            Method::GET,
            &link,
            &[("accept", "application/json;odata.metadata=none")],
            "",
        )
        .await
        .json();
    assert!(none["@odata.context"].is_null());
    assert!(none["@odata.deltaLink"].is_string());
    assert!(
        none["value"][1]["@odata.context"].is_string(),
        "a removal says what it is at any metadata level: {none}"
    );
}

async fn paging(h: Harness) {
    let link = h.link("Tasks").await;
    for (id, title) in [("a", "1"), ("b", "2"), ("c", "3")] {
        h.done(
            Method::PATCH,
            &format!("Tasks('{id}')"),
            json!({"Title": title}),
        )
        .await;
    }
    h.done(Method::POST, "Tasks", json!({"Id": "d", "Title": "4"}))
        .await;
    h.done(Method::DELETE, "Tasks('d')", json!({})).await;
    let mut seen = Vec::new();
    let mut uri = link.clone();
    let mut pages = 0;
    let last = loop {
        let r = h.tracked(&uri, "odata.maxpagesize=2").await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text());
        let doc = r.json();
        assert!(doc["value"].as_array().unwrap().len() <= 2);
        seen.extend(entries(&doc, "Id"));
        pages += 1;
        if doc["@odata.nextLink"].is_null() {
            break doc;
        }
        assert!(doc["@odata.deltaLink"].is_null());
        uri = relative(&doc["@odata.nextLink"]);
    };
    assert_eq!(pages, 3);
    assert_eq!(
        seen,
        [
            hit("a"),
            hit("b"),
            hit("c"),
            gone("Tasks('d')", "changed"),
            gone("Tasks('d')", "deleted")
        ],
        "pages split the log, and a later page corrects an earlier one"
    );
    let next = relative(&last["@odata.deltaLink"]);
    assert_eq!(entries(&h.app.ok(&next).await, "Id"), []);

    let r = h
        .tracked("Tasks", "odata.track-changes,odata.maxpagesize=2")
        .await;
    let doc = r.json();
    assert_eq!(doc["value"].as_array().unwrap().len(), 2);
    assert!(doc["@odata.deltaLink"].is_null(), "only on the last page");
    let second = relative(&doc["@odata.nextLink"]);
    assert!(second.contains("$deltatoken="), "{second}");
    h.done(Method::PATCH, "Tasks('a')", json!({"Title": "During"}))
        .await;
    let doc = h.app.ok(&second).await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 1);
    let link = relative(&doc["@odata.deltaLink"]);
    assert_eq!(
        entries(&h.app.ok(&link).await, "Id"),
        [hit("a")],
        "the link starts at the first page"
    );
}

async fn tokens(h: Harness) {
    for token in ["x", "1.2", "1.2.x", "i.x", ""] {
        let uri = format!("Tasks?$deltatoken={token}");
        assert_eq!(h.app.status(&uri).await, 400, "{uri}");
    }
    for uri in [
        "People?$deltatoken=1",
        "Tasks?$deltatoken=1&$top=1",
        "Tasks?$deltatoken=1&$expand=Labels",
        "Tasks/$count?$deltatoken=1",
        "Tasks('a')?$deltatoken=1",
    ] {
        assert_eq!(h.app.status(uri).await, 400, "{uri}");
    }

    let link = h.link("Tasks").await;
    h.done(Method::PATCH, "Tasks('a')", json!({"Title": "Old"}))
        .await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    h.done(Method::PATCH, "Tasks('c')", json!({"Title": "New"}))
        .await;
    let removed = h.db.prune_change_log(Duration::from_secs(1)).await.unwrap();
    assert_eq!(removed, 1, "older changes go");
    assert_eq!(
        h.app.status(&link).await,
        410,
        "the link reached past the log"
    );
    let again =
        h.db.prune_change_log(Duration::from_secs(3600))
            .await
            .unwrap();
    assert_eq!(again, 0);
    assert_eq!(h.app.status(&link).await, 410, "and stays expired");
    let fresh = h.link("Tasks").await;
    h.done(Method::PATCH, "Tasks('a')", json!({"Title": "Later"}))
        .await;
    assert_eq!(entries(&h.app.ok(&fresh).await, "Id"), [hit("a")]);
}

async fn outside_writers(h: Harness) {
    let link = h.link("Labels").await;
    let session = Session::from(h.db.clone());
    record_change(
        &session,
        "Labels",
        None,
        "(Code=2,TaskId='a')",
        ChangeKind::Updated,
    )
    .await
    .unwrap();
    record_change(&session, "Labels", None, "not a key", ChangeKind::Updated)
        .await
        .unwrap();
    let doc = h.app.ok(&link).await;
    assert_eq!(
        doc["value"].as_array().unwrap().len(),
        1,
        "any key order works, and a bad row is skipped: {doc}"
    );
    assert_eq!(doc["value"][0]["Body"], "two");
}

async fn security(h: Harness) {
    let hr = App {
        router: h
            .app
            .router
            .clone()
            .layer(Extension(Claims::default().with_group("hr"))),
    };
    let link = h.link("Tasks").await;
    h.done(Method::PATCH, "Tasks('a')", json!({"Title": "Seen"}))
        .await;
    let doc = h.app.ok(&link).await;
    assert!(doc["value"][0]["Budget"].is_null(), "{doc}");
    assert_eq!(hr.ok(&link).await["value"][0]["Budget"], 100);

    let plain = h
        .tracked("Tasks?$filter=Budget gt 1", "odata.track-changes")
        .await;
    assert_eq!(plain.status.as_u16(), 403);
    let token = link.rsplit('=').next().unwrap();
    assert_eq!(
        h.app
            .status(&format!("Tasks?$filter=Budget gt 1&$deltatoken={token}"))
            .await,
        403,
        "a token doesn't reach past visibility"
    );
    assert_eq!(
        h.app.status(&format!("Archive?$deltatoken={token}")).await,
        403
    );
    let hidden = h
        .write(
            Method::PATCH,
            "Projects(1)",
            json!({"Tasks@delta": [{"Id": "a", "Budget": 1}]}),
        )
        .await;
    assert_eq!(hidden.status.as_u16(), 403);
    let hidden = h
        .write(
            Method::PATCH,
            "Tasks",
            json!({"value": [{"Id": "a", "Budget": 1}]}),
        )
        .await;
    assert_eq!(hidden.status.as_u16(), 403);
    assert_eq!(hr.ok("Tasks('a')").await["Budget"], 100);
    let injected = "Tasks?$deltatoken=1%3BDROP%20TABLE%20task";
    assert_eq!(h.app.status(injected).await, 400);
    assert_eq!(
        h.app.ok("Tasks").await["value"].as_array().unwrap().len(),
        3
    );
}

async fn nested_delta(h: Harness) {
    let link = h.link("Tasks").await;
    h.done(
        Method::PATCH,
        "Projects(1)",
        json!({
            "Title": "Changed",
            "Tasks@delta": [
                {"Id": "a", "Title": "Updated"},
                {"Id": "new", "Title": "Created"},
                {"@id": "Tasks('c')", "Title": "Moved in"},
                {"@removed": {"reason": "changed"}, "@id": "Tasks('b')"},
            ],
        }),
    )
    .await;
    assert_eq!(h.app.ok("Projects(1)").await["Title"], "Changed");
    let mut ids = h.app.column("Projects(1)/Tasks?$orderby=Id", "Id").await;
    ids.sort_by_key(|v| v.as_str().map(str::to_owned));
    assert_eq!(ids, [json!("a"), json!("c"), json!("new")]);
    assert_eq!(h.app.ok("Tasks('a')").await["Title"], "Updated");
    assert_eq!(h.app.ok("Tasks('c')").await["Title"], "Moved in");
    let b = h.app.ok("Tasks('b')").await;
    assert_eq!(b["Title"], "Second", "unlinked, not deleted");
    assert!(b["ProjectId"].is_null());
    assert_eq!(
        entries(&h.app.ok(&link).await, "Id").len(),
        4,
        "delta payloads reach delta links"
    );

    h.done(
        Method::PATCH,
        "Tasks('a')",
        json!({"Labels@delta": [
            {"@removed": {"reason": "deleted"}, "TaskId": "a", "Code": 1},
            {"@odata.context": "#Labels/$deletedEntity", "id": "Labels(TaskId='a',Code=2)", "reason": "changed"},
            {"Code": 3, "Body": "three"},
        ]}),
    )
    .await;
    let labels = h.app.column("Tasks('a')/Labels", "Code").await;
    assert_eq!(
        labels,
        [json!(3)],
        "a key column can't be unlinked, so it goes"
    );

    h.done(
        Method::PATCH,
        "Projects(1)",
        json!({"Members@odata.delta": [
            {"@odata.id": "People(3)"},
            {"@id": "People(3)"},
            {"@removed": {}, "@id": "People(1)"},
            {"@removed": {"reason": "changed"}, "@id": "People(2)"},
            {"@removed": {"reason": "deleted"}, "@id": "People(9)"},
        ]}),
    )
    .await;
    assert_eq!(h.app.column("Projects(1)/Members", "Id").await, [json!(3)]);
    assert_eq!(
        h.app.ok("People(1)").await["Name"],
        "Ann",
        "only the link went"
    );

    h.done(
        Method::PATCH,
        "Projects(2)",
        json!({"Tasks@delta": [{"@removed": {"reason": "deleted"}, "@id": "Tasks('a')"}]}),
    )
    .await;
    assert_eq!(
        h.app.status("Tasks('a')").await,
        200,
        "not a member, so untouched"
    );

    for (uri, body) in [
        ("Tasks('b')", json!({"Project@delta": []})),
        ("Projects(1)", json!({"Nope@delta": []})),
        ("Projects(1)", json!({"Tasks@delta": {}})),
        ("Projects(1)", json!({"Tasks@delta": [1]})),
        (
            "Projects(1)",
            json!({"Tasks@delta": [{"@removed": {"reason": "lost"}, "Id": "c"}]}),
        ),
        ("Projects(1)", json!({"Tasks@delta": [{"@removed": {}}]})),
        (
            "Projects(1)",
            json!({"Tasks@delta": [{"@id": "Projects(1)"}]}),
        ),
        (
            "Projects(1)",
            json!({"Tasks@delta": [{"@id": "Tasks('c')", "Id": "x"}]}),
        ),
    ] {
        let r = h.write(Method::PATCH, uri, body.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}: {}", r.text());
    }
    let r = h
        .write(
            Method::POST,
            "Projects",
            json!({"Id": 7, "Title": "T", "Tasks@delta": []}),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "insert has nothing to change"
    );
    assert_eq!(h.app.status("Projects(7)").await, 404);

    let r = h
        .write(
            Method::PATCH,
            "Projects(1)",
            json!({"Title": "Lost", "Tasks@delta": [
                {"Id": "a", "Title": "Lost too"},
                {"Id": "z"},
            ]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "z has no title");
    assert_eq!(
        h.app.ok("Projects(1)").await["Title"],
        "Changed",
        "all or nothing"
    );
    assert_eq!(h.app.ok("Tasks('a')").await["Title"], "Updated");
}

async fn collection_patch(h: Harness) {
    let link = h.link("Tasks").await;
    let body = json!({
        "@context": "#$delta",
        "value": [
            {"Id": "a", "Title": "Updated"},
            {"Id": "new", "Title": "Created"},
            {"@id": "Tasks('c')", "Priority": 2},
            {"@removed": {"reason": "deleted"}, "@id": "Tasks('zz')"},
        ],
    });
    let r = h.write(Method::PATCH, "Tasks", body).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(h.app.ok("Tasks('a')").await["Title"], "Updated");
    assert_eq!(h.app.ok("Tasks('new')").await["Title"], "Created");
    assert_eq!(h.app.ok("Tasks('c')").await["Priority"], 2);
    assert_eq!(
        h.app.ok("Tasks('b')").await["Title"],
        "Second",
        "not named, not touched"
    );
    assert_eq!(entries(&h.app.ok(&link).await, "Id").len(), 3);

    let r = h
        .app
        .request(
            Method::PATCH,
            "Tasks",
            &[
                ("content-type", "application/json"),
                ("prefer", "return=representation"),
            ],
            &json!({"value": [
                {"Id": "a", "Title": "Shown"},
                {"@removed": {"reason": "deleted"}, "Id": "new"},
            ]})
            .to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(
        r.header("preference-applied"),
        Some("return=representation")
    );
    let doc = r.json();
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#Tasks/$delta")
    );
    assert_eq!(
        entries(&doc, "Id"),
        [hit("a"), gone("Tasks('new')", "deleted")]
    );
    assert_eq!(doc["value"][0]["Title"], "Shown");
    assert_eq!(h.app.status("Tasks('new')").await, 404);

    h.done(
        Method::PATCH,
        "Projects(2)/Tasks",
        json!({"value": [{"Id": "made", "Title": "Under 2"}]}),
    )
    .await;
    assert_eq!(h.app.ok("Tasks('made')").await["ProjectId"], 2);

    let r = h
        .write(
            Method::PATCH,
            "Tasks",
            json!({"value": [{"Id": "a", "Title": "Lost"}, {"Id": "z"}]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        h.app.ok("Tasks('a')").await["Title"],
        "Shown",
        "all or nothing"
    );

    for body in [
        json!({}),
        json!({"value": {}}),
        json!({"value": [1]}),
        json!({"value": [{"@id": "Tasks('a')"}]}),
        json!({"value": [{"@removed": {}}]}),
    ] {
        let r = h.write(Method::PATCH, "Tasks", body.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}: {}", r.text());
    }
    let r = h.write(Method::PUT, "Tasks", json!({"value": []})).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    let r = h
        .write(Method::PATCH, "Tasks/$count", json!({"value": []}))
        .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
}

async fn metadata(h: Harness) {
    let xml = h.app.get("$metadata").await.text();
    let tracked = "<Annotation Term=\"Capabilities.ChangeTracking\"><Record><PropertyValue Property=\"Supported\"><Bool>true</Bool></PropertyValue>";
    assert_eq!(
        xml.matches(tracked).count(),
        3,
        "tracked sets the caller can see: {xml}"
    );
}

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let pool = common::sqlite_pool(super::SCHEMA).await;
                super::$name(super::Harness::new(pool.into()).await).await;
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
                super::$name(super::Harness::new(pool.into()).await).await;
            )*
            let pool = server.pool(super::SCHEMA).await;
            super::commits_out_of_order(super::Harness::new(pool.into()).await).await;
            let pool = server.pool(super::SCHEMA).await;
            super::trigger_from_the_readme(pool).await;
        }
    };
}

/// A transaction that started first can commit last. Its change must still reach a client
/// whose read happened in between.
async fn commits_out_of_order(h: Harness) {
    let slow = Session::begin(&h.db).await.unwrap();
    record_change(&slow, "Tasks", None, "('a')", ChangeKind::Updated)
        .await
        .unwrap();
    h.done(Method::PATCH, "Tasks('b')", json!({"Title": "Fast"}))
        .await;
    let link = h.link("Tasks").await;
    slow.commit().await.unwrap();
    let doc = h.app.ok(&link).await;
    assert!(
        entries(&doc, "Id").contains(&hit("a")),
        "the late commit is in the delta: {doc}"
    );
}

/// The trigger the README offers for writers outside the service, taken from the README.
async fn trigger_from_the_readme(pool: sqlx::PgPool) {
    let readme = include_str!("../../../README.md");
    let start = readme.find("CREATE FUNCTION log_task").unwrap();
    let end = start + readme[start..].find("```").unwrap();
    let h = Harness::new(pool.clone().into()).await;
    let run = |sql: String| {
        let pool = pool.clone();
        async move {
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    run(readme[start..end].to_owned()).await;
    let link = h.link("Tasks").await;
    run("INSERT INTO task (id, title) VALUES ('t''x', 'Outside')".to_owned()).await;
    run("UPDATE task SET title = 'Outside too' WHERE id = 'a'".to_owned()).await;
    run("DELETE FROM task WHERE id = 'c'".to_owned()).await;
    let doc = h.app.ok(&link).await;
    assert_eq!(
        entries(&doc, "Id"),
        [hit("t'x"), hit("a"), gone("Tasks('c')", "deleted")],
        "{doc}"
    );
}

mod run {
    use super::common;

    scenarios!(
        first_read,
        changes,
        every_write_is_logged,
        defining_query,
        paging,
        tokens,
        outside_writers,
        security,
        nested_delta,
        collection_patch,
        metadata,
    );
}

#[derive(ODataEntity)]
#[odata(table = "task", set = "Protected", track_changes)]
pub struct Protected {
    #[odata(key)]
    pub id: String,
    #[odata(protected(claim = "owner"))]
    pub title: String,
}

#[tokio::test]
async fn hidden_rows_cannot_be_tracked() {
    let db: Database = common::sqlite_pool(SCHEMA).await.into();
    let e = Service::builder("D").entity::<Protected>().build(db).err();
    assert!(
        e.is_some_and(|e| e.to_string().contains("cannot be tracked")),
        "a removal would reveal the keys of rows the caller can't see"
    );
}
