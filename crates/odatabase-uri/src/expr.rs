//! Expression parser for `$filter`, `$orderby` and function arguments, binding as it goes.

use crate::UriError;
use crate::ast::*;
use crate::lexer::{Token, tokenize};
use odatabase_edm::{Model, PrimitiveKind, TypeKind, Value};
use std::collections::HashMap;

/// Named parameter aliases supplied by the query string, already parsed to expressions.
pub type Aliases = HashMap<String, Expr>;

/// The type an implicit path starts from.
#[derive(Clone, Debug)]
pub enum Scope {
    Entity(String),
    Complex(String),
    /// No implicit resource: paths are errors, as in function call arguments.
    None,
}

pub struct ExprParser<'a> {
    pub model: &'a Model,
    pub tokens: Vec<Token>,
    pub pos: usize,
    pub scope: Scope,
    pub vars: Vec<(String, ExprType)>,
    pub aliases: &'a Aliases,
    pub depth: u32,
    pub max_depth: u32,
}

const FUNCTIONS: [(&str, Func, u8, u8); 28] = [
    ("contains", Func::Contains, 2, 2),
    ("startswith", Func::StartsWith, 2, 2),
    ("endswith", Func::EndsWith, 2, 2),
    ("length", Func::Length, 1, 1),
    ("indexof", Func::IndexOf, 2, 2),
    ("substring", Func::Substring, 2, 3),
    ("tolower", Func::ToLower, 1, 1),
    ("toupper", Func::ToUpper, 1, 1),
    ("trim", Func::Trim, 1, 1),
    ("concat", Func::Concat, 2, 2),
    ("matchesPattern", Func::MatchesPattern, 2, 2),
    ("year", Func::Year, 1, 1),
    ("month", Func::Month, 1, 1),
    ("day", Func::Day, 1, 1),
    ("hour", Func::Hour, 1, 1),
    ("minute", Func::Minute, 1, 1),
    ("second", Func::Second, 1, 1),
    ("fractionalseconds", Func::FractionalSeconds, 1, 1),
    ("totalseconds", Func::TotalSeconds, 1, 1),
    ("totaloffsetminutes", Func::TotalOffsetMinutes, 1, 1),
    ("date", Func::Date, 1, 1),
    ("time", Func::Time, 1, 1),
    ("now", Func::Now, 0, 0),
    ("maxdatetime", Func::MaxDateTime, 0, 0),
    ("mindatetime", Func::MinDateTime, 0, 0),
    ("round", Func::Round, 1, 1),
    ("floor", Func::Floor, 1, 1),
    ("ceiling", Func::Ceiling, 1, 1),
];

const UNSUPPORTED_FUNCTIONS: [&str; 7] = [
    "geo.distance",
    "geo.length",
    "geo.intersects",
    "hassubset",
    "hassubsequence",
    "case",
    "$root",
];

/// Parses a complete expression over `scope`.
pub fn parse_expression(
    model: &Model,
    text: &str,
    scope: Scope,
    aliases: &Aliases,
    max_depth: u32,
) -> Result<Expr, UriError> {
    let mut p = ExprParser {
        model,
        tokens: tokenize(text)?,
        pos: 0,
        scope,
        vars: Vec::new(),
        aliases,
        depth: 0,
        max_depth,
    };
    let e = p.or_expr()?;
    p.expect_end()?;
    Ok(e)
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            Some(Token::Ident(s)) => Some(s),
            _ => None,
        }
    }

    fn accept(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn accept_word(&mut self, word: &str) -> bool {
        if self.peek_word() == Some(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Token, what: &str) -> Result<(), UriError> {
        if self.accept(t) {
            Ok(())
        } else {
            Err(UriError::bad_request(format!("expected {what}")))
        }
    }

    pub fn expect_end(&self) -> Result<(), UriError> {
        match self.peek() {
            None => Ok(()),
            Some(t) => Err(UriError::bad_request(format!("unexpected token {t:?}"))),
        }
    }

    fn enter(&mut self) -> Result<(), UriError> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err(UriError::bad_request("expression is nested too deeply"));
        }
        Ok(())
    }

    pub fn or_expr(&mut self) -> Result<Expr, UriError> {
        self.enter()?;
        let mut left = self.and_expr()?;
        while self.accept_word("or") {
            let right = self.and_expr()?;
            left = self.logical(BinaryOp::Or, left, right)?;
        }
        self.depth -= 1;
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, UriError> {
        let mut left = self.not_expr()?;
        while self.accept_word("and") {
            let right = self.not_expr()?;
            left = self.logical(BinaryOp::And, left, right)?;
        }
        Ok(left)
    }

    fn logical(&self, op: BinaryOp, l: Expr, r: Expr) -> Result<Expr, UriError> {
        require_bool(&l)?;
        require_bool(&r)?;
        Ok(Expr::Binary(op, Box::new(l), Box::new(r)))
    }

    fn not_expr(&mut self) -> Result<Expr, UriError> {
        if self.accept_word("not") {
            self.enter()?;
            let inner = self.not_expr()?;
            self.depth -= 1;
            require_bool(&inner)?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr, UriError> {
        let left = self.additive()?;
        let op = match self.peek_word() {
            Some("eq") => BinaryOp::Eq,
            Some("ne") => BinaryOp::Ne,
            Some("gt") => BinaryOp::Gt,
            Some("ge") => BinaryOp::Ge,
            Some("lt") => BinaryOp::Lt,
            Some("le") => BinaryOp::Le,
            Some("has") => BinaryOp::Has,
            Some("in") => {
                self.pos += 1;
                return self.in_list(left);
            }
            _ => return Ok(left),
        };
        self.pos += 1;
        let right = self.additive()?;
        let (left, right) = self.unify(left, right)?;
        if op != BinaryOp::Has {
            self.check_comparable(op, &left, &right)?;
        } else if !matches!(left.ty(), ExprType::Enum(_)) {
            return Err(UriError::bad_request("has requires an enum on the left"));
        }
        Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
    }

    fn in_list(&mut self, left: Expr) -> Result<Expr, UriError> {
        self.expect(&Token::LParen, "( after in")?;
        let mut items = Vec::new();
        if !self.accept(&Token::RParen) {
            loop {
                let item = self.or_expr()?;
                let (_, item) = self.unify(left.clone(), item)?;
                self.check_comparable(BinaryOp::Eq, &left, &item)?;
                items.push(item);
                if !self.accept(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RParen, ") after in list")?;
        }
        Ok(Expr::In(Box::new(left), items))
    }

    /// Fits a literal to the declared type on the other side of a comparison.
    fn unify(&self, l: Expr, r: Expr) -> Result<(Expr, Expr), UriError> {
        let fit = |lit: Expr, other: &ExprType| -> Result<Expr, UriError> {
            let Expr::Literal(v) = &lit else {
                return Ok(lit);
            };
            match (other, v) {
                (ExprType::Enum(name), Value::String(member)) => {
                    self.enum_value(name, member).map(Expr::Literal)
                }
                (ExprType::Enum(name), Value::Int32(_) | Value::Int64(_)) => {
                    let e = self.enum_type(name)?;
                    let n = v.as_i64().unwrap_or_default();
                    let member = e.member_name(n).ok_or_else(|| {
                        UriError::bad_request(format!("{n} is not a member of {name}"))
                    })?;
                    Ok(Expr::Literal(Value::Enum {
                        type_name: name.clone(),
                        member: member.to_owned(),
                    }))
                }
                (ExprType::Primitive(k), _) if k.is_numeric() && v.as_i64().is_some() => {
                    Ok(v.coerce(*k).map_or(lit.clone(), Expr::Literal))
                }
                _ => Ok(lit),
            }
        };
        let (lt, rt) = (l.ty(), r.ty());
        Ok((fit(l, &rt)?, fit(r, &lt)?))
    }

    fn check_comparable(&self, op: BinaryOp, l: &Expr, r: &Expr) -> Result<(), UriError> {
        use PrimitiveKind as P;
        let (lt, rt) = (l.ty(), r.ty());
        let ok = match (&lt, &rt) {
            (ExprType::Null, _) | (_, ExprType::Null) => true,
            (ExprType::Primitive(a), ExprType::Primitive(b)) => {
                a == b
                    || (a.is_numeric() && b.is_numeric())
                    || matches!(
                        (a, b),
                        (P::Date, P::DateTimeOffset) | (P::DateTimeOffset, P::Date)
                    )
            }
            (ExprType::Enum(a), ExprType::Enum(b)) => a == b,
            _ => false,
        };
        if !ok {
            return Err(UriError::bad_request(format!(
                "cannot compare {lt:?} with {rt:?}"
            )));
        }
        let ordered = !matches!(op, BinaryOp::Eq | BinaryOp::Ne);
        if ordered && (lt.is_boolean() && lt != ExprType::Null) {
            return Err(UriError::bad_request("booleans have no order"));
        }
        Ok(())
    }

    fn additive(&mut self) -> Result<Expr, UriError> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek_word() {
                Some("add") => BinaryOp::Add,
                Some("sub") => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.multiplicative()?;
            left = arithmetic(op, left, right)?;
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, UriError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek_word() {
                Some("mul") => BinaryOp::Mul,
                Some("div") => BinaryOp::Div,
                Some("divby") => BinaryOp::DivBy,
                Some("mod") => BinaryOp::Mod,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.unary()?;
            left = arithmetic(op, left, right)?;
        }
    }

    fn unary(&mut self) -> Result<Expr, UriError> {
        if self.accept(&Token::Minus) {
            self.enter()?;
            let inner = self.unary()?;
            self.depth -= 1;
            if !inner.ty().is_numeric() {
                return Err(UriError::bad_request("cannot negate a non-numeric value"));
            }
            return Ok(Expr::Negate(Box::new(inner)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, UriError> {
        match self.peek().cloned() {
            None => Err(UriError::bad_request("unexpected end of expression")),
            Some(Token::LParen) => {
                self.pos += 1;
                let e = self.or_expr()?;
                self.expect(&Token::RParen, ")")?;
                Ok(e)
            }
            Some(Token::Literal(v)) => {
                self.pos += 1;
                Ok(Expr::Literal(v))
            }
            Some(Token::Alias(name)) => {
                self.pos += 1;
                Ok(self
                    .aliases
                    .get(&name)
                    .cloned()
                    .unwrap_or(Expr::Literal(Value::Null)))
            }
            Some(Token::TypedLiteral(type_name, member)) => {
                self.pos += 1;
                if type_name.starts_with("geography") || type_name.starts_with("geometry") {
                    return Err(UriError::not_implemented("geospatial literals"));
                }
                self.enum_value(self.model.local(&type_name), &member)
                    .map(Expr::Literal)
            }
            Some(Token::Ident(name)) => self.identifier(&name),
            Some(t) => Err(UriError::bad_request(format!("unexpected token {t:?}"))),
        }
    }

    fn enum_type(&self, name: &str) -> Result<&odatabase_edm::EnumType, UriError> {
        self.model
            .enum_type(name)
            .ok_or_else(|| UriError::bad_request(format!("unknown enum type {name}")))
    }

    fn enum_value(&self, type_name: &str, member: &str) -> Result<Value, UriError> {
        let e = self.enum_type(type_name)?;
        if member.split(',').all(|m| {
            let m = m.trim();
            e.member_value(m).is_some()
                || m.parse::<i64>().is_ok_and(|n| e.member_name(n).is_some())
        }) {
            Ok(Value::Enum {
                type_name: e.name.clone(),
                member: member.to_owned(),
            })
        } else {
            Err(UriError::bad_request(format!(
                "{member} is not a member of {type_name}"
            )))
        }
    }

    fn identifier(&mut self, name: &str) -> Result<Expr, UriError> {
        let is_call = self.tokens.get(self.pos + 1) == Some(&Token::LParen);
        if is_call {
            match name {
                "cast" => return self.cast(),
                "isof" => return self.isof(),
                _ => {}
            }
            if let Some((_, f, min, max)) = FUNCTIONS.iter().find(|(n, ..)| *n == name) {
                self.pos += 1;
                return self.builtin(*f, *min, *max);
            }
            if UNSUPPORTED_FUNCTIONS.contains(&name) {
                return Err(UriError::not_implemented(format!("function {name}")));
            }
            if name.contains('.') {
                self.pos += 1;
                return self.composable(name);
            }
            return Err(UriError::bad_request(format!("unknown function {name}")));
        }
        self.pos += 1;
        self.path(name)
    }

    fn arguments(&mut self) -> Result<Vec<Expr>, UriError> {
        self.expect(&Token::LParen, "(")?;
        let mut args = Vec::new();
        if self.accept(&Token::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.or_expr()?);
            if !self.accept(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RParen, ")")?;
        Ok(args)
    }

    fn builtin(&mut self, f: Func, min: u8, max: u8) -> Result<Expr, UriError> {
        let args = self.arguments()?;
        if args.len() < usize::from(min) || args.len() > usize::from(max) {
            return Err(UriError::bad_request(format!(
                "wrong argument count for {f:?}"
            )));
        }
        check_arguments(f, &args)?;
        Ok(Expr::Call(f, args))
    }

    fn type_name(&mut self) -> Result<ExprType, UriError> {
        match self.peek().cloned() {
            Some(Token::Ident(name)) => {
                self.pos += 1;
                self.resolve_type(&name)
            }
            _ => Err(UriError::bad_request("expected a type name")),
        }
    }

    fn resolve_type(&self, name: &str) -> Result<ExprType, UriError> {
        if let Some(k) = PrimitiveKind::from_edm_name(name) {
            return Ok(ExprType::Primitive(k));
        }
        let local = self.model.local(name);
        if self.model.entity_types.contains_key(local) {
            return Ok(ExprType::Entity(local.to_owned()));
        }
        if self.model.complex_types.contains_key(local) {
            return Ok(ExprType::Complex(local.to_owned()));
        }
        if self.model.enum_types.contains_key(local) {
            return Ok(ExprType::Enum(local.to_owned()));
        }
        Err(UriError::bad_request(format!("unknown type {name}")))
    }

    fn cast(&mut self) -> Result<Expr, UriError> {
        self.pos += 2;
        let first = self.peek().cloned();
        let e = if let Some(Token::Ident(n)) = &first
            && self.tokens.get(self.pos + 1) == Some(&Token::RParen)
            && n.contains('.')
        {
            let ty = self.type_name()?;
            let it = self.it_path()?;
            Expr::Cast(Box::new(it), ty)
        } else {
            let value = self.or_expr()?;
            self.expect(&Token::Comma, ",")?;
            let ty = self.type_name()?;
            Expr::Cast(Box::new(value), ty)
        };
        self.expect(&Token::RParen, ")")?;
        Ok(e)
    }

    fn isof(&mut self) -> Result<Expr, UriError> {
        self.pos += 2;
        let first = self.peek().cloned();
        let e = if let Some(Token::Ident(n)) = &first
            && self.tokens.get(self.pos + 1) == Some(&Token::RParen)
            && n.contains('.')
        {
            Expr::IsOf(None, self.type_name()?)
        } else {
            let value = self.or_expr()?;
            self.expect(&Token::Comma, ",")?;
            Expr::IsOf(Some(Box::new(value)), self.type_name()?)
        };
        self.expect(&Token::RParen, ")")?;
        Ok(e)
    }

    /// `Ns.Function(a=1,b=Prop)` used inside an expression.
    fn composable(&mut self, qualified: &str) -> Result<Expr, UriError> {
        let local = self.model.local(qualified);
        let overloads = self
            .model
            .functions
            .get(local)
            .ok_or_else(|| UriError::bad_request(format!("unknown function {qualified}")))?;
        self.expect(&Token::LParen, "(")?;
        let mut args = Vec::new();
        if !self.accept(&Token::RParen) {
            loop {
                let Some(Token::Ident(name)) = self.peek().cloned() else {
                    return Err(UriError::bad_request("expected a parameter name"));
                };
                self.pos += 1;
                self.expect(&Token::Equals, "=")?;
                args.push((name, self.or_expr()?));
                if !self.accept(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RParen, ")")?;
        }
        let op = overloads
            .iter()
            .filter(|o| !o.is_bound)
            .find(|o| {
                o.parameters.len() == args.len()
                    && o.parameters
                        .iter()
                        .all(|p| args.iter().any(|(n, _)| *n == p.name))
            })
            .ok_or_else(|| UriError::bad_request(format!("no overload of {qualified} matches")))?;
        let Some(ret) = &op.return_type else {
            return Err(UriError::bad_request(format!(
                "{qualified} returns nothing"
            )));
        };
        if ret.collection {
            return Err(UriError::not_implemented(
                "collection functions in expressions",
            ));
        }
        let ty = match &ret.kind {
            odatabase_edm::OperationTypeKind::Value(TypeKind::Primitive(k)) => {
                ExprType::Primitive(*k)
            }
            odatabase_edm::OperationTypeKind::Value(TypeKind::Enum(n)) => ExprType::Enum(n.clone()),
            _ => {
                return Err(UriError::not_implemented(
                    "structured function results in expressions",
                ));
            }
        };
        Ok(Expr::Function {
            name: local.to_owned(),
            args,
            ty,
        })
    }

    fn it_path(&self) -> Result<Expr, UriError> {
        let ty = self.scope_type()?;
        Ok(Expr::Path(Path {
            root: PathRoot::It,
            steps: Vec::new(),
            ty,
        }))
    }

    fn scope_type(&self) -> Result<ExprType, UriError> {
        match &self.scope {
            Scope::Entity(n) => Ok(ExprType::Entity(n.clone())),
            Scope::Complex(n) => Ok(ExprType::Complex(n.clone())),
            Scope::None => Err(UriError::bad_request("no resource in scope")),
        }
    }

    fn path(&mut self, first: &str) -> Result<Expr, UriError> {
        let var = self.vars.iter().rev().find(|(n, _)| n == first).cloned();
        let (root, mut ty, mut steps) = if let Some((name, ty)) = var {
            (PathRoot::Var(name), ty, Vec::new())
        } else if first == "$it" {
            (PathRoot::It, self.scope_type()?, Vec::new())
        } else if first.starts_with('$') {
            return Err(UriError::bad_request(format!("unexpected {first}")));
        } else {
            let scope = self.scope_type()?;
            let (step, ty) = self.step(&scope, first)?;
            (PathRoot::It, ty, vec![step])
        };
        while self.accept(&Token::Slash) {
            let Some(Token::Ident(name)) = self.peek().cloned() else {
                return Err(UriError::bad_request("expected a path segment"));
            };
            self.pos += 1;
            if matches!(name.as_str(), "any" | "all") {
                return self.lambda(&name, Path { root, steps, ty });
            }
            if name == "$count" {
                if ty.element().is_none() {
                    return Err(UriError::bad_request("$count needs a collection"));
                }
                steps.push(Step::Count);
                return Ok(Expr::Path(Path {
                    root,
                    steps,
                    ty: ExprType::Primitive(PrimitiveKind::Int64),
                }));
            }
            let (step, next) = self.step(&ty, &name)?;
            steps.push(step);
            ty = next;
        }
        Ok(Expr::Path(Path { root, steps, ty }))
    }

    /// Resolves one path step against `current`.
    fn step(&self, current: &ExprType, name: &str) -> Result<(Step, ExprType), UriError> {
        if name.contains('.') {
            let ty = self.resolve_type(name)?;
            let ExprType::Entity(derived) = &ty else {
                return Err(UriError::bad_request(format!("cannot cast to {name}")));
            };
            let ty = match current {
                ExprType::Collection(_) => ExprType::Collection(Box::new(ty.clone())),
                _ => ty.clone(),
            };
            return Ok((Step::Cast(derived.clone()), ty));
        }
        let unknown = || UriError::bad_request(format!("unknown property {name}"));
        match current {
            ExprType::Entity(t) => {
                if let Some(p) = self.model.property(t, name) {
                    return Ok((Step::Property(name.to_owned()), property_type(p)));
                }
                if let Some(n) = self.model.navigation(t, name) {
                    let target = ExprType::Entity(n.target.clone());
                    let ty = if n.collection {
                        ExprType::Collection(Box::new(target))
                    } else {
                        target
                    };
                    return Ok((Step::Navigation(name.to_owned()), ty));
                }
                Err(unknown())
            }
            ExprType::Complex(t) => {
                let c = self.model.complex_type(t).ok_or_else(unknown)?;
                let p = c
                    .properties
                    .iter()
                    .find(|p| p.name == name)
                    .ok_or_else(unknown)?;
                Ok((Step::Property(name.to_owned()), property_type(p)))
            }
            ExprType::Collection(_) => Err(UriError::bad_request(format!(
                "{name}: a collection can only be used with any, all or $count"
            ))),
            _ => Err(unknown()),
        }
    }

    fn lambda(&mut self, name: &str, source: Path) -> Result<Expr, UriError> {
        let kind = if name == "any" {
            LambdaKind::Any
        } else {
            LambdaKind::All
        };
        let Some(element) = source.ty.element().cloned() else {
            return Err(UriError::bad_request(format!("{name} needs a collection")));
        };
        self.expect(&Token::LParen, "(")?;
        if kind == LambdaKind::Any && self.accept(&Token::RParen) {
            return Ok(Expr::Lambda {
                source,
                kind,
                var: None,
                body: None,
            });
        }
        let Some(Token::Ident(var)) = self.peek().cloned() else {
            return Err(UriError::bad_request("expected a lambda variable"));
        };
        self.pos += 1;
        self.expect(&Token::Colon, ":")?;
        if self.vars.iter().any(|(n, _)| *n == var) {
            return Err(UriError::bad_request(format!(
                "lambda variable {var} is already in use"
            )));
        }
        self.vars.push((var.clone(), element));
        let body = self.or_expr()?;
        self.vars.pop();
        require_bool(&body)?;
        self.expect(&Token::RParen, ")")?;
        Ok(Expr::Lambda {
            source,
            kind,
            var: Some(var),
            body: Some(Box::new(body)),
        })
    }
}

pub fn property_type(p: &odatabase_edm::Property) -> ExprType {
    let inner = match &p.kind {
        TypeKind::Primitive(k) => ExprType::Primitive(*k),
        TypeKind::Enum(n) => ExprType::Enum(n.clone()),
        TypeKind::Complex(n) => ExprType::Complex(n.clone()),
    };
    if p.collection.is_some() {
        ExprType::Collection(Box::new(inner))
    } else {
        inner
    }
}

fn require_bool(e: &Expr) -> Result<(), UriError> {
    if e.ty().is_boolean() {
        Ok(())
    } else {
        Err(UriError::bad_request(format!(
            "expected a boolean, found {:?}",
            e.ty()
        )))
    }
}

fn arithmetic(op: BinaryOp, l: Expr, r: Expr) -> Result<Expr, UriError> {
    use PrimitiveKind as P;
    let allowed = |t: &ExprType| match t {
        ExprType::Null => true,
        ExprType::Primitive(k) => {
            k.is_numeric()
                || (matches!(op, BinaryOp::Add | BinaryOp::Sub)
                    && matches!(k, P::Date | P::DateTimeOffset | P::Duration))
        }
        _ => false,
    };
    if !allowed(&l.ty()) || !allowed(&r.ty()) {
        return Err(UriError::bad_request(format!(
            "{op:?} is not defined for {:?} and {:?}",
            l.ty(),
            r.ty()
        )));
    }
    Ok(Expr::Binary(op, Box::new(l), Box::new(r)))
}

fn check_arguments(f: Func, args: &[Expr]) -> Result<(), UriError> {
    use PrimitiveKind as P;
    let is = |e: &Expr, kinds: &[P]| match e.ty() {
        ExprType::Null => true,
        ExprType::Primitive(k) => kinds.contains(&k),
        _ => false,
    };
    let all_strings = || args.iter().all(|a| a.ty().is_string());
    let ok = match f {
        Func::Contains
        | Func::StartsWith
        | Func::EndsWith
        | Func::Length
        | Func::IndexOf
        | Func::ToLower
        | Func::ToUpper
        | Func::Trim
        | Func::Concat
        | Func::MatchesPattern => all_strings(),
        Func::Substring => {
            args[0].ty().is_string()
                && args[1..]
                    .iter()
                    .all(|a| matches!(a.ty(), ExprType::Primitive(k) if k.is_integer()))
        }
        Func::Year | Func::Month | Func::Day => is(&args[0], &[P::Date, P::DateTimeOffset]),
        Func::Hour | Func::Minute | Func::Second | Func::FractionalSeconds => {
            is(&args[0], &[P::TimeOfDay, P::DateTimeOffset])
        }
        Func::TotalSeconds => is(&args[0], &[P::Duration]),
        Func::TotalOffsetMinutes | Func::Date | Func::Time => is(&args[0], &[P::DateTimeOffset]),
        Func::Round | Func::Floor | Func::Ceiling => args[0].ty().is_numeric(),
        Func::Now | Func::MaxDateTime | Func::MinDateTime => true,
    };
    if ok {
        Ok(())
    } else {
        Err(UriError::bad_request(format!(
            "invalid argument types for {f:?}"
        )))
    }
}
