use odatabase_edm::{PrimitiveKind, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct ODataUri {
    pub path: Vec<Segment>,
    pub options: QueryOptions,
}

impl ODataUri {
    /// The type addressed by the full path.
    pub fn target(&self) -> &Target {
        self.path.last().map_or(&Target::Root, Segment::target)
    }
}

/// What a path prefix refers to.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Root,
    EntityCollection(String),
    Entity(String),
    Complex(String),
    ComplexCollection(String),
    Primitive(ExprType),
    PrimitiveCollection(ExprType),
    Count,
    Ref {
        collection: bool,
    },
    /// `$value` of a primitive property or of a media entity.
    RawValue,
    Metadata,
    Batch,
    ActionResult,
    Void,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    Metadata,
    Batch,
    EntitySet {
        name: String,
        target: Target,
    },
    Singleton {
        name: String,
        target: Target,
    },
    Key {
        values: Vec<(String, Value)>,
        target: Target,
    },
    Navigation {
        name: String,
        target: Target,
    },
    Property {
        name: String,
        target: Target,
    },
    Cast {
        type_name: String,
        target: Target,
    },
    Count,
    Value,
    Ref,
    Function {
        name: String,
        overload: usize,
        args: Vec<(String, Expr)>,
        target: Target,
    },
    Action {
        name: String,
        overload: usize,
        target: Target,
    },
}

impl Segment {
    pub fn target(&self) -> &Target {
        static COUNT: Target = Target::Count;
        static VALUE: Target = Target::RawValue;
        static METADATA: Target = Target::Metadata;
        static BATCH: Target = Target::Batch;
        static REF: Target = Target::Ref { collection: false };
        match self {
            Self::Metadata => &METADATA,
            Self::Batch => &BATCH,
            Self::Count => &COUNT,
            Self::Value => &VALUE,
            Self::Ref => &REF,
            Self::EntitySet { target, .. }
            | Self::Singleton { target, .. }
            | Self::Key { target, .. }
            | Self::Navigation { target, .. }
            | Self::Property { target, .. }
            | Self::Cast { target, .. }
            | Self::Function { target, .. }
            | Self::Action { target, .. } => target,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryOptions {
    pub filter: Option<Expr>,
    pub select: Option<Vec<SelectItem>>,
    pub expand: Vec<ExpandItem>,
    pub orderby: Vec<OrderItem>,
    pub top: Option<u64>,
    pub skip: Option<u64>,
    pub count: bool,
    pub search: Option<SearchExpr>,
    pub skiptoken: Option<String>,
    pub format: Option<String>,
    /// Query options without a `$` prefix, passed through to `nextLink`.
    pub custom: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectItem {
    Star,
    /// A property path. Multiple steps address the inside of complex types.
    Path(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpandItem {
    /// Complex properties leading to the navigation property.
    pub prefix: Vec<String>,
    pub navigation: String,
    pub target: String,
    pub collection: bool,
    pub options: QueryOptions,
    /// Recursive expansion. `None` inside means `$levels=max`.
    pub levels: Option<Levels>,
    pub count_only: bool,
    pub ref_only: bool,
    /// Came from `$expand=*`, which skips what the caller may not see.
    pub from_star: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Levels {
    Number(u32),
    Max,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchExpr {
    Term(String),
    Not(Box<SearchExpr>),
    And(Box<SearchExpr>, Box<SearchExpr>),
    Or(Box<SearchExpr>, Box<SearchExpr>),
}

/// Result type of an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprType {
    Null,
    Primitive(PrimitiveKind),
    Enum(String),
    Complex(String),
    Entity(String),
    Collection(Box<ExprType>),
}

impl ExprType {
    pub fn is_boolean(&self) -> bool {
        matches!(self, Self::Primitive(PrimitiveKind::Boolean) | Self::Null)
    }

    pub fn is_string(&self) -> bool {
        matches!(self, Self::Primitive(PrimitiveKind::String) | Self::Null)
    }

    pub fn is_numeric(&self) -> bool {
        match self {
            Self::Primitive(k) => k.is_numeric(),
            Self::Null => true,
            _ => false,
        }
    }

    pub fn element(&self) -> Option<&ExprType> {
        match self {
            Self::Collection(inner) => Some(inner),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    DivBy,
    Mod,
    Has,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LambdaKind {
    Any,
    All,
}

/// The built-in functions of OData 4.0 and the ones added by 4.01.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Contains,
    StartsWith,
    EndsWith,
    Length,
    IndexOf,
    Substring,
    ToLower,
    ToUpper,
    Trim,
    Concat,
    MatchesPattern,
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    FractionalSeconds,
    TotalSeconds,
    TotalOffsetMinutes,
    Date,
    Time,
    Now,
    MaxDateTime,
    MinDateTime,
    Round,
    Floor,
    Ceiling,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathRoot {
    /// The resource the expression applies to, possibly written as `$it`.
    It,
    /// A variable bound by an enclosing lambda.
    Var(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Property(String),
    Navigation(String),
    Cast(String),
    Count,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    pub root: PathRoot,
    pub steps: Vec<Step>,
    pub ty: ExprType,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Value),
    Path(Path),
    Not(Box<Expr>),
    Negate(Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    In(Box<Expr>, Vec<Expr>),
    Call(Func, Vec<Expr>),
    Lambda {
        source: Path,
        kind: LambdaKind,
        var: Option<String>,
        body: Option<Box<Expr>>,
    },
    Cast(Box<Expr>, ExprType),
    IsOf(Option<Box<Expr>>, ExprType),
    /// A composable function called from an expression.
    Function {
        name: String,
        args: Vec<(String, Expr)>,
        ty: ExprType,
    },
}

impl Expr {
    pub fn ty(&self) -> ExprType {
        let boolean = ExprType::Primitive(PrimitiveKind::Boolean);
        match self {
            Self::Literal(v) => match v {
                Value::Null => ExprType::Null,
                Value::Enum { type_name, .. } => ExprType::Enum(type_name.clone()),
                other => other.kind().map_or(ExprType::Null, ExprType::Primitive),
            },
            Self::Path(p) => p.ty.clone(),
            Self::Not(_) | Self::In(..) | Self::Lambda { .. } | Self::IsOf(..) => boolean,
            Self::Negate(e) => e.ty(),
            Self::Binary(op, l, r) => match op {
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Mod => {
                    promote(&l.ty(), &r.ty())
                }
                BinaryOp::Div => promote(&l.ty(), &r.ty()),
                BinaryOp::DivBy => ExprType::Primitive(PrimitiveKind::Decimal),
                _ => boolean,
            },
            Self::Call(f, args) => call_type(*f, args),
            Self::Cast(_, t) => t.clone(),
            Self::Function { ty, .. } => ty.clone(),
        }
    }
}

/// The result type of arithmetic, following the numeric promotion rules.
pub fn promote(l: &ExprType, r: &ExprType) -> ExprType {
    use PrimitiveKind as P;
    let rank = |k: PrimitiveKind| match k {
        P::Byte | P::SByte => 1,
        P::Int16 => 2,
        P::Int32 => 3,
        P::Int64 => 4,
        P::Decimal => 5,
        P::Single => 6,
        P::Double => 7,
        _ => 0,
    };
    match (l, r) {
        (ExprType::Primitive(a), ExprType::Primitive(b)) => {
            if rank(*a) >= rank(*b) {
                ExprType::Primitive(*a)
            } else {
                ExprType::Primitive(*b)
            }
        }
        (ExprType::Null, other) | (other, _) => other.clone(),
    }
}

fn call_type(f: Func, args: &[Expr]) -> ExprType {
    use PrimitiveKind as P;
    let kind = match f {
        Func::Contains | Func::StartsWith | Func::EndsWith | Func::MatchesPattern => P::Boolean,
        Func::Length | Func::IndexOf => P::Int32,
        Func::Substring | Func::ToLower | Func::ToUpper | Func::Trim | Func::Concat => P::String,
        Func::Year | Func::Month | Func::Day | Func::Hour | Func::Minute | Func::Second => P::Int32,
        Func::TotalOffsetMinutes => P::Int32,
        Func::FractionalSeconds | Func::TotalSeconds => P::Decimal,
        Func::Date => P::Date,
        Func::Time => P::TimeOfDay,
        Func::Now | Func::MaxDateTime | Func::MinDateTime => P::DateTimeOffset,
        Func::Round | Func::Floor | Func::Ceiling => {
            return args
                .first()
                .map_or(ExprType::Primitive(P::Double), Expr::ty);
        }
    };
    ExprType::Primitive(kind)
}
