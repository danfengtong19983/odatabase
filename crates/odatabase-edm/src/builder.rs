//! Assembles a [`Model`] from derived types and validates it.

use crate::model::*;
use crate::value::PrimitiveKind;
use std::collections::{BTreeSet, HashSet};

/// The model is inconsistent. Reported when the service is built, never at request time.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid model: {0}")]
pub struct ModelError(pub String);

/// Implemented by `#[derive(ODataEntity)]`.
pub trait EdmEntity {
    /// Qualified name of the entity type.
    fn edm_name() -> &'static str;
    /// Adds the type, its set and the types it refers to.
    fn register(builder: &mut ModelBuilder);
}

/// Maps a Rust field type to its Edm type. Derived for complex types and enums.
pub trait EdmField {
    /// The Edm type, registering complex and enum types on first use.
    fn field_type(builder: &mut ModelBuilder) -> FieldType;
}

/// The Edm view of a Rust field.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldType {
    /// The type.
    pub kind: TypeKind,
    /// From `Option<T>`.
    pub nullable: bool,
    /// From `Vec<T>`.
    pub collection: bool,
}

impl FieldType {
    /// A non-nullable, single-valued primitive.
    pub fn primitive(kind: PrimitiveKind) -> Self {
        Self {
            kind: TypeKind::Primitive(kind),
            nullable: false,
            collection: false,
        }
    }
}

macro_rules! edm_field {
    ($($ty:ty => $kind:ident),* $(,)?) => {$(
        impl EdmField for $ty {
            fn field_type(_: &mut ModelBuilder) -> FieldType {
                FieldType::primitive(PrimitiveKind::$kind)
            }
        }
    )*};
}

edm_field! {
    bool => Boolean,
    u8 => Byte,
    i8 => SByte,
    i16 => Int16,
    i32 => Int32,
    i64 => Int64,
    f32 => Single,
    f64 => Double,
    String => String,
    rust_decimal::Decimal => Decimal,
    chrono::NaiveDate => Date,
    chrono::NaiveTime => TimeOfDay,
    chrono::DateTime<chrono::Utc> => DateTimeOffset,
    chrono::DateTime<chrono::FixedOffset> => DateTimeOffset,
    chrono::TimeDelta => Duration,
    uuid::Uuid => Guid,
    Binary => Binary,
    GeographyPoint => GeographyPoint,
    GeographyLineString => GeographyLineString,
    GeographyPolygon => GeographyPolygon,
    GeometryPoint => GeometryPoint,
    GeometryLineString => GeometryLineString,
    GeometryPolygon => GeometryPolygon,
}

/// `Edm.Binary` field. A plain `Vec<u8>` would read as a collection of bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Binary(pub Vec<u8>);

macro_rules! geo_field {
    ($($name:ident),*) => {$(
        /// Geospatial field, held as a GeoJSON document.
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct $name(pub String);
    )*};
}

geo_field!(
    GeographyPoint,
    GeographyLineString,
    GeographyPolygon,
    GeometryPoint,
    GeometryLineString,
    GeometryPolygon
);

impl<T: EdmField> EdmField for Option<T> {
    fn field_type(b: &mut ModelBuilder) -> FieldType {
        FieldType {
            nullable: true,
            ..T::field_type(b)
        }
    }
}

impl<T: EdmField> EdmField for Vec<T> {
    fn field_type(b: &mut ModelBuilder) -> FieldType {
        FieldType {
            collection: true,
            ..T::field_type(b)
        }
    }
}

/// Field marker for a navigation property. Carries no data: rows are model-described.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nav;

/// Collects types, then validates them in [`ModelBuilder::build`].
#[derive(Debug)]
pub struct ModelBuilder {
    model: Model,
    visited: HashSet<&'static str>,
}

impl ModelBuilder {
    /// An empty model with the schema `namespace`.
    pub fn new(namespace: impl Into<String>) -> Self {
        let namespace = namespace.into();
        let container = "Container".to_owned();
        Self {
            model: Model {
                namespace,
                container,
                ..Model::default()
            },
            visited: HashSet::new(),
        }
    }

    /// Names the entity container.
    pub fn container(mut self, name: impl Into<String>) -> Self {
        self.model.container = name.into();
        self
    }

    /// Registers an entity type and everything it reaches.
    pub fn entity<T: EdmEntity>(mut self) -> Self {
        self.register_entity::<T>();
        self
    }

    #[doc(hidden)]
    pub fn register_entity<T: EdmEntity>(&mut self) {
        if self.visited.insert(T::edm_name()) {
            T::register(self);
        }
    }

    #[doc(hidden)]
    /// True on the first call for `name`. Stops recursion through cyclic types.
    pub fn first_visit(&mut self, name: &'static str) -> bool {
        self.visited.insert(name)
    }

    #[doc(hidden)]
    pub fn add_entity_type(&mut self, t: EntityType) {
        self.model.entity_types.insert(t.name.clone(), t);
    }

    #[doc(hidden)]
    pub fn add_complex_type(&mut self, t: ComplexType) {
        self.model.complex_types.insert(t.name.clone(), t);
    }

    #[doc(hidden)]
    pub fn add_enum_type(&mut self, t: EnumType) {
        self.model.enum_types.insert(t.name.clone(), t);
    }

    #[doc(hidden)]
    pub fn add_entity_set(&mut self, s: EntitySet) {
        self.model.entity_sets.insert(s.name.clone(), s);
    }

    #[doc(hidden)]
    pub fn add_singleton(&mut self, s: Singleton) {
        self.model.singletons.insert(s.name.clone(), s);
    }

    #[doc(hidden)]
    pub fn add_function(&mut self, op: Operation) {
        self.model
            .functions
            .entry(op.name.clone())
            .or_default()
            .push(op);
    }

    #[doc(hidden)]
    pub fn add_action(&mut self, op: Operation) {
        self.model
            .actions
            .entry(op.name.clone())
            .or_default()
            .push(op);
    }

    /// Adds a function overload.
    pub fn function(mut self, op: Operation) -> Self {
        self.add_function(op);
        self
    }

    /// Adds an action overload.
    pub fn action(mut self, op: Operation) -> Self {
        self.add_action(op);
        self
    }

    /// Adds an annotation.
    pub fn annotation(mut self, a: Annotation) -> Self {
        self.model.annotations.push(a);
        self
    }

    #[doc(hidden)]
    pub fn model_mut(&mut self) -> &mut Model {
        &mut self.model
    }

    /// Validates and returns the model.
    pub fn build(self) -> Result<Model, ModelError> {
        validate(&self.model)?;
        Ok(self.model)
    }
}

fn validate(m: &Model) -> Result<(), ModelError> {
    let bail = |msg: String| Err(ModelError(msg));
    for t in m.entity_types.values() {
        if let Some(base) = &t.base_type
            && !m.entity_types.contains_key(base)
        {
            return bail(format!("{}: unknown base type {base}", t.name));
        }
        if m.type_chain(&t.name)
            .last()
            .is_some_and(|root| root.base_type.is_some())
        {
            return bail(format!("{}: inheritance cycle", t.name));
        }
        let key = m.key_of(&t.name);
        if key.is_empty() && !t.is_abstract {
            return bail(format!("{}: entity type has no key", t.name));
        }
        if t.base_type.is_some() && !t.key.is_empty() {
            return bail(format!(
                "{}: a derived type cannot redeclare the key",
                t.name
            ));
        }
        for k in key {
            let Some(p) = m.property(&t.name, k) else {
                return bail(format!("{}: key property {k} does not exist", t.name));
            };
            if p.nullable {
                return bail(format!("{}: key property {k} must not be nullable", t.name));
            }
            if p.collection.is_some() || matches!(p.kind, TypeKind::Complex(_)) {
                return bail(format!("{}: key property {k} must be a scalar", t.name));
            }
        }
        if let Some(e) = &t.etag
            && !t.properties.iter().any(|p| &p.name == e)
        {
            return bail(format!("{}: etag property {e} does not exist", t.name));
        }
        let mut names = BTreeSet::new();
        for p in m.all_properties(&t.name) {
            if !names.insert(p.name.as_str()) {
                return bail(format!("{}: duplicate property {}", t.name, p.name));
            }
        }
        for n in m.all_navigation(&t.name) {
            if !names.insert(n.name.as_str()) {
                return bail(format!("{}: duplicate property {}", t.name, n.name));
            }
        }
        for p in &t.properties {
            validate_property(m, &t.name, p)?;
        }
        for n in &t.navigation {
            let Some(target) = m.entity_type(&n.target) else {
                return bail(format!(
                    "{}.{}: unknown target {}",
                    t.name, n.name, n.target
                ));
            };
            if n.join.is_empty() && n.join_table.is_none() {
                return bail(format!("{}.{}: no join columns", t.name, n.name));
            }
            if let Some(partner) = &n.partner
                && m.navigation(&target.name, partner).is_none()
            {
                return bail(format!("{}.{}: unknown partner {partner}", t.name, n.name));
            }
        }
    }
    for t in m.complex_types.values() {
        for p in &t.properties {
            validate_property(m, &t.name, p)?;
        }
    }
    for s in m.entity_sets.values() {
        if !m.entity_types.contains_key(&s.entity_type) {
            return bail(format!("set {}: unknown type {}", s.name, s.entity_type));
        }
    }
    for s in m.singletons.values() {
        if !m.entity_types.contains_key(&s.entity_type) {
            return bail(format!(
                "singleton {}: unknown type {}",
                s.name, s.entity_type
            ));
        }
        if m.entity_sets.contains_key(&s.name) {
            return bail(format!("{}: name used by a set and a singleton", s.name));
        }
    }
    for (name, ops) in m.functions.iter().chain(m.actions.iter()) {
        let mut signatures = HashSet::new();
        for op in ops {
            if op.is_bound && op.parameters.is_empty() {
                return bail(format!("{name}: bound operation without binding parameter"));
            }
            let signature: Vec<String> = op.parameters.iter().map(|p| p.ty.type_name(m)).collect();
            if !signatures.insert((op.is_bound, signature)) {
                return bail(format!("{name}: duplicate overload"));
            }
        }
    }
    Ok(())
}

fn validate_property(m: &Model, owner: &str, p: &Property) -> Result<(), ModelError> {
    match &p.kind {
        TypeKind::Complex(n) if !m.complex_types.contains_key(n) => Err(ModelError(format!(
            "{owner}.{}: unknown complex type {n}",
            p.name
        ))),
        TypeKind::Enum(n) if !m.enum_types.contains_key(n) => Err(ModelError(format!(
            "{owner}.{}: unknown enum type {n}",
            p.name
        ))),
        _ => Ok(()),
    }
}

/// `first_name` to `FirstName`, the default Edm name of a Rust field.
pub fn edm_case(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for part in field.split('_').filter(|p| !p.is_empty()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// `BusinessPartner` to `BusinessPartners`, the default entity set name.
pub fn plural(name: &str) -> String {
    if let Some(stem) = name.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        return format!("{stem}ies");
    }
    if name.ends_with('s') || name.ends_with('x') || name.ends_with("ch") || name.ends_with("sh") {
        return format!("{name}es");
    }
    format!("{name}s")
}

/// `BusinessPartner` to `business_partner`, the default table name.
pub fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_key(name: &str) -> Property {
        Property {
            nullable: false,
            ..Property::new(name, "id", TypeKind::Primitive(PrimitiveKind::String))
        }
    }

    fn entity(name: &str) -> EntityType {
        EntityType {
            name: name.into(),
            table: "t".into(),
            key: vec!["Id".into()],
            properties: vec![string_key("Id")],
            ..EntityType::default()
        }
    }

    #[test]
    fn names() {
        assert_eq!(edm_case("first_name"), "FirstName");
        assert_eq!(plural("Company"), "Companies");
        assert_eq!(plural("Day"), "Days");
        assert_eq!(plural("Address"), "Addresses");
        assert_eq!(snake_case("BusinessPartner"), "business_partner");
    }

    #[test]
    fn valid_model_builds() {
        let mut b = ModelBuilder::new("T");
        b.add_entity_type(entity("A"));
        assert!(b.build().is_ok());
    }

    #[test]
    fn nullable_key_is_rejected() {
        let mut b = ModelBuilder::new("T");
        let mut t = entity("A");
        t.properties[0].nullable = true;
        b.add_entity_type(t);
        assert!(b.build().unwrap_err().0.contains("must not be nullable"));
    }

    #[test]
    fn missing_key_is_rejected() {
        let mut b = ModelBuilder::new("T");
        let mut t = entity("A");
        t.key.clear();
        b.add_entity_type(t);
        assert!(b.build().unwrap_err().0.contains("no key"));
    }

    #[test]
    fn unknown_navigation_target_is_rejected() {
        let mut b = ModelBuilder::new("T");
        let mut t = entity("A");
        t.navigation.push(NavigationProperty {
            name: "B".into(),
            target: "Missing".into(),
            join: vec![ColumnPair::new("id", "a_id")],
            ..NavigationProperty::default()
        });
        b.add_entity_type(t);
        assert!(b.build().unwrap_err().0.contains("unknown target"));
    }

    #[test]
    fn inheritance_cycle_is_rejected() {
        let mut b = ModelBuilder::new("T");
        let mut x = entity("X");
        x.key.clear();
        x.base_type = Some("Y".into());
        let mut y = entity("Y");
        y.key.clear();
        y.base_type = Some("X".into());
        b.add_entity_type(x);
        b.add_entity_type(y);
        assert!(b.build().unwrap_err().0.contains("cycle"));
    }
}
