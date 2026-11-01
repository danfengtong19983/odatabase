use odatabase::edm::{
    Model, ModelBuilder, Nav, Operation, OperationType, OperationTypeKind, Parameter,
    PrimitiveKind, TypeKind, Value,
};
use odatabase::{ODataComplex, ODataEntity, ODataEnum};
use odatabase_uri::*;

#[derive(ODataEnum)]
#[allow(dead_code)]
enum Abc {
    A = 1,
    B = 2,
}

#[derive(ODataComplex)]
#[allow(dead_code)]
struct Address {
    city: Option<String>,
}

#[derive(ODataEntity)]
#[allow(dead_code)]
struct Partner {
    #[odata(key)]
    id: String,
    age: Option<i32>,
    score: Option<f64>,
    born: Option<chrono::NaiveDate>,
    class: Option<Abc>,
    #[odata(embedded)]
    address: Address,
    #[odata(collection(table = "c", owner(id = "pid"), value_column = "v"))]
    tags: Vec<String>,
    #[odata(has_many(target = Role, on(id = "partner_id")))]
    roles: Nav,
}

#[derive(ODataEntity)]
#[allow(dead_code)]
struct Role {
    #[odata(key)]
    partner_id: String,
    #[odata(key)]
    code: i32,
    #[odata(belongs_to(target = Partner, on(partner_id = "id")))]
    partner: Nav,
}

#[derive(ODataEntity)]
#[odata(base = Partner, discriminator(column = "k", value = "O"))]
#[allow(dead_code)]
struct Org {
    name: String,
}

fn int_type() -> OperationType {
    OperationType {
        kind: OperationTypeKind::Value(TypeKind::Primitive(PrimitiveKind::Int32)),
        collection: false,
        nullable: false,
    }
}

fn model() -> Model {
    ModelBuilder::new("T")
        .entity::<Partner>()
        .entity::<Org>()
        .function(Operation {
            name: "Double".into(),
            is_composable: true,
            parameters: vec![Parameter {
                name: "x".into(),
                ty: int_type(),
            }],
            return_type: Some(int_type()),
            ..Operation::default()
        })
        .action(Operation {
            name: "Reset".into(),
            ..Operation::default()
        })
        .build()
        .unwrap()
}

fn ok(path: &str, query: &str) -> ODataUri {
    parse(&model(), path, query).unwrap_or_else(|e| panic!("{path}?{query}: {e}"))
}

fn fail(path: &str, query: &str) -> UriError {
    parse(&model(), path, query).expect_err(&format!("{path}?{query} should fail"))
}

fn filter(text: &str) -> Expr {
    ok("Partners", &format!("$filter={text}"))
        .options
        .filter
        .unwrap()
}

#[test]
fn paths() {
    assert_eq!(
        *ok("Partners", "").target(),
        Target::EntityCollection("Partner".into())
    );
    assert_eq!(
        *ok("Partners('1')", "").target(),
        Target::Entity("Partner".into())
    );
    assert_eq!(
        *ok("/Partners('1')/", "").target(),
        Target::Entity("Partner".into())
    );
    assert_eq!(
        *ok("Partners('1')/Roles", "").target(),
        Target::EntityCollection("Role".into())
    );
    assert_eq!(
        *ok("Roles(PartnerId='1',Code=2)/Partner/Age", "").target(),
        Target::Primitive(ExprType::Primitive(PrimitiveKind::Int32))
    );
    assert_eq!(*ok("Partners/$count", "").target(), Target::Count);
    assert_eq!(
        *ok("Partners('1')/Age/$value", "").target(),
        Target::RawValue
    );
    assert_eq!(
        *ok("Partners('1')/Address", "").target(),
        Target::Complex("Address".into())
    );
    assert_eq!(*ok("$metadata", "").target(), Target::Metadata);
    assert_eq!(*ok("", "").target(), Target::Root);
    assert_eq!(
        *ok("Partners/T.Org", "").target(),
        Target::EntityCollection("Org".into())
    );
    assert_eq!(
        *ok("Partners('1')/T.Org/Name", "").target(),
        Target::Primitive(ExprType::Primitive(PrimitiveKind::String))
    );
}

#[test]
fn key_values_are_typed_and_keep_slashes_in_strings() {
    let uri = ok("Partners('a/b''c')", "");
    let Segment::Key { values, .. } = &uri.path[1] else {
        panic!()
    };
    assert_eq!(values[0], ("Id".to_owned(), Value::String("a/b'c".into())));
    let uri = ok("Partners('a%2Fb')", "");
    let Segment::Key { values, .. } = &uri.path[1] else {
        panic!()
    };
    assert_eq!(values[0].1, Value::String("a/b".into()));
    let uri = ok("Roles(Code=7,PartnerId='x')", "");
    let Segment::Key { values, .. } = &uri.path[1] else {
        panic!()
    };
    assert_eq!(values[0], ("Code".to_owned(), Value::Int32(7)));
    let uri = ok("Partners(Id='a=b')", "");
    let Segment::Key { values, .. } = &uri.path[1] else {
        panic!()
    };
    assert_eq!(values[0].1, Value::String("a=b".into()));
    let uri = ok("Partners('a=b')", "");
    let Segment::Key { values, .. } = &uri.path[1] else {
        panic!()
    };
    assert_eq!(values[0].1, Value::String("a=b".into()));
}

#[test]
fn path_errors_use_the_right_status() {
    assert_eq!(fail("Nope", "").status, 404);
    assert_eq!(fail("Partners('1')/Nope", "").status, 404);
    assert_eq!(fail("Partners/Age", "").status, 400);
    assert_eq!(fail("Partners(1)", "").status, 400);
    assert_eq!(fail("Roles(Code=1)", "").status, 400);
    assert_eq!(fail("Partners('1')/$count", "").status, 400);
    assert_eq!(fail("Partners/$count/x", "").status, 400);
    assert_eq!(fail("$all", "").status, 501);
    assert_eq!(fail("Partners", "$apply=aggregate(Age)").status, 501);
    assert_eq!(fail("Partners", "$nope=1").status, 400);
    assert_eq!(fail("Partners", "$top=-1").status, 400);
    assert_eq!(fail("Partners", "$top=1&$top=2").status, 400);
}

#[test]
fn filter_precedence() {
    let Expr::Binary(BinaryOp::Or, _, r) = filter("Age eq 1 or Age eq 2 and not (Age eq 3)") else {
        panic!("or binds loosest");
    };
    assert!(matches!(*r, Expr::Binary(BinaryOp::And, ..)));
    let Expr::Binary(BinaryOp::Gt, l, _) = filter("Age add 2 mul 3 gt 1") else {
        panic!()
    };
    let Expr::Binary(BinaryOp::Add, _, r) = *l else {
        panic!("mul binds tighter")
    };
    assert!(matches!(*r, Expr::Binary(BinaryOp::Mul, ..)));
}

#[test]
fn filter_literals() {
    for (text, expected) in [
        ("Age eq 5", Value::Int32(5)),
        ("Score eq 1e3", Value::Double(1000.0)),
        ("Id eq 'O''Neil'", Value::String("O'Neil".into())),
        (
            "Born eq 2026-10-31",
            Value::Date("2026-10-31".parse().unwrap()),
        ),
        ("Age eq null", Value::Null),
    ] {
        let Expr::Binary(_, _, r) = filter(text) else {
            panic!("{text}")
        };
        assert_eq!(*r, Expr::Literal(expected), "{text}");
    }
}

#[test]
fn plus_is_accepted_as_a_space() {
    assert!(matches!(filter("Age+eq+5"), Expr::Binary(BinaryOp::Eq, ..)));
}

#[test]
fn enum_literals_resolve_against_the_model() {
    let Expr::Binary(_, _, r) = filter("Class eq T.Abc'A'") else {
        panic!()
    };
    assert_eq!(
        *r,
        Expr::Literal(Value::Enum {
            type_name: "Abc".into(),
            member: "A".into()
        })
    );
    let Expr::Binary(_, _, r) = filter("Class eq 'B'") else {
        panic!()
    };
    assert_eq!(
        *r,
        Expr::Literal(Value::Enum {
            type_name: "Abc".into(),
            member: "B".into()
        })
    );
    assert_eq!(fail("Partners", "$filter=Class eq T.Abc'Z'").status, 400);
}

#[test]
fn navigation_lambda_and_count() {
    let Expr::Lambda {
        source,
        kind,
        var,
        body,
    } = filter("Roles/any(r: r/Code gt 1)")
    else {
        panic!()
    };
    assert_eq!(kind, LambdaKind::Any);
    assert_eq!(var.as_deref(), Some("r"));
    assert_eq!(source.steps, [Step::Navigation("Roles".into())]);
    let Expr::Binary(_, l, _) = *body.unwrap() else {
        panic!()
    };
    let Expr::Path(p) = *l else { panic!() };
    assert_eq!(p.root, PathRoot::Var("r".into()));
    assert_eq!(p.steps, [Step::Property("Code".into())]);

    assert!(matches!(
        filter("Roles/any()"),
        Expr::Lambda { body: None, .. }
    ));
    assert!(matches!(
        filter("Tags/any(t: t eq 'x')"),
        Expr::Lambda { .. }
    ));
    assert!(matches!(
        filter("Roles/$count gt 1"),
        Expr::Binary(BinaryOp::Gt, ..)
    ));
    assert!(matches!(filter("Address/City eq 'B'"), Expr::Binary(..)));
    assert!(matches!(filter("T.Org/Name eq 'B'"), Expr::Binary(..)));
    assert!(matches!(
        filter("Roles/all(r: r/Partner/Roles/any(q: q/Code eq r/Code))"),
        Expr::Lambda {
            kind: LambdaKind::All,
            ..
        }
    ));
}

#[test]
fn functions_and_in() {
    assert!(matches!(
        filter("contains(Id,'x')"),
        Expr::Call(Func::Contains, _)
    ));
    assert!(matches!(filter("year(Born) eq 2020"), Expr::Binary(..)));
    assert!(matches!(filter("Age in (1,2,3)"), Expr::In(..)));
    assert!(matches!(filter("T.Double(x=Age) gt 1"), Expr::Binary(..)));
    assert!(matches!(filter("isof(T.Org)"), Expr::IsOf(None, _)));
    assert!(matches!(
        filter("cast(Age,Edm.String) eq '1'"),
        Expr::Binary(..)
    ));
}

#[test]
fn filter_type_errors() {
    for text in [
        "Age",
        "Age eq 'x'",
        "contains(Age,'x')",
        "Nope eq 1",
        "Roles eq 1",
        "Roles/Code eq 1",
        "Age and true",
        "Id add 1 eq 2",
        "Age eq",
        "(Age eq 1",
        "Age eq 1 extra",
        "length('a','b') eq 1",
        "Tags/any(Age: true) and Roles/any(r: r/any(q: true))",
        "Roles/any(r: Roles/any(r: true))",
    ] {
        assert_eq!(
            fail("Partners", &format!("$filter={text}")).status,
            400,
            "{text}"
        );
    }
    assert_eq!(
        fail("Partners", "$filter=geo.distance(Id,'x') lt 1").status,
        501
    );
}

#[test]
fn deep_nesting_is_rejected_rather_than_overflowing_the_stack() {
    let deep = format!("{}true{}", "(".repeat(200), ")".repeat(200));
    assert_eq!(fail("Partners", &format!("$filter={deep}")).status, 400);
    let nots = format!("{}true", "not ".repeat(100_000));
    assert_eq!(fail("Partners", &format!("$filter={nots}")).status, 400);
    let minus = format!("{}1 eq 1", "- ".repeat(100_000));
    assert_eq!(fail("Partners", &format!("$filter={minus}")).status, 400);
}

#[test]
fn aliases() {
    let uri = ok("Partners", "$filter=Age gt @a&@a=5");
    let Some(Expr::Binary(_, _, r)) = uri.options.filter else {
        panic!()
    };
    assert_eq!(*r, Expr::Literal(Value::Int32(5)));
    let uri = ok("Partners", "$filter=Age gt @missing");
    let Some(Expr::Binary(_, _, r)) = uri.options.filter else {
        panic!()
    };
    assert_eq!(*r, Expr::Literal(Value::Null), "an absent alias is null");
}

#[test]
fn orderby_select_top_skip() {
    let o = ok(
        "Partners",
        "$orderby=Age desc,Id,Address/City asc&$top=5&$skip=2&$count=true",
    )
    .options;
    assert_eq!(o.orderby.len(), 3);
    assert!(o.orderby[0].descending);
    assert!(!o.orderby[1].descending);
    assert_eq!((o.top, o.skip, o.count), (Some(5), Some(2), true));
    let o = ok("Partners", "$orderby=Roles/$count desc").options;
    assert!(o.orderby[0].descending);
    let o = ok("Partners", "$select=Id,Address/City,*").options;
    assert_eq!(
        o.select.unwrap(),
        vec![
            SelectItem::Path(vec!["Id".into()]),
            SelectItem::Path(vec!["Address".into(), "City".into()]),
            SelectItem::Star
        ]
    );
    assert_eq!(fail("Partners", "$select=Nope").status, 400);
    assert_eq!(fail("Partners", "$orderby=Roles").status, 400);
}

#[test]
fn expand() {
    let o = ok(
        "Partners",
        "$expand=Roles($filter=Code gt 1;$select=Code;$orderby=Code desc;$top=2;$expand=Partner($expand=Roles/$count))",
    )
    .options;
    let roles = &o.expand[0];
    assert_eq!(roles.navigation, "Roles");
    assert!(roles.collection);
    assert_eq!(roles.options.top, Some(2));
    assert!(roles.options.filter.is_some());
    let partner = &roles.options.expand[0];
    assert_eq!(partner.target, "Partner");
    assert!(partner.options.expand[0].count_only);

    assert_eq!(ok("Partners", "$expand=*").options.expand.len(), 1);
    assert!(ok("Partners", "$expand=Roles/$ref").options.expand[0].ref_only);
    assert_eq!(
        ok("Roles", "$expand=Partner($levels=2)").options.expand[0].levels,
        Some(Levels::Number(2))
    );
    assert_eq!(
        ok("Roles", "$expand=Partner($levels=max)").options.expand[0].levels,
        Some(Levels::Max)
    );
    assert_eq!(fail("Partners", "$expand=Nope").status, 400);
    assert_eq!(fail("Roles", "$expand=Partner($top=1)").status, 400);
    let deep = "Roles($expand=Partner(".repeat(6) + "$expand=Roles" + &"))".repeat(6);
    assert_eq!(fail("Partners", &format!("$expand={deep}")).status, 400);
}

#[test]
fn search() {
    let o = ok("Partners", "$search=mountain AND \"red bike\" OR NOT cheap").options;
    let Some(SearchExpr::Or(l, r)) = o.search else {
        panic!()
    };
    assert!(matches!(*l, SearchExpr::And(..)));
    assert!(matches!(*r, SearchExpr::Not(_)));
    let o = ok("Partners", "$search=a b").options;
    assert!(matches!(o.search, Some(SearchExpr::And(..))));
    assert_eq!(fail("Partners", "$search=NOT").status, 400);
    assert_eq!(fail("Partners", "$search=\"open").status, 400);
    assert_eq!(
        fail("Partners", &format!("$search={}a", "NOT ".repeat(100_000))).status,
        400
    );
}

#[test]
fn operations() {
    let uri = ok("Double(x=2)", "");
    assert!(matches!(uri.path[0], Segment::Function { .. }));
    assert_eq!(
        *uri.target(),
        Target::Primitive(ExprType::Primitive(PrimitiveKind::Int32))
    );
    let uri = ok("Double(x=@v)", "@v=3");
    let Segment::Function { args, .. } = &uri.path[0] else {
        panic!()
    };
    assert_eq!(args[0].1, Expr::Literal(Value::Int32(3)));
    assert_eq!(*ok("Reset", "").target(), Target::ActionResult);
    assert_eq!(fail("Double(y=2)", "").status, 404);
}

#[test]
fn custom_options_are_kept() {
    let o = ok("Partners", "sap-client=100").options;
    assert_eq!(o.custom, [("sap-client".to_owned(), "100".to_owned())]);
}
