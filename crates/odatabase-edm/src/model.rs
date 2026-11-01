//! The service model: everything `$metadata` publishes, plus the database mapping behind it.

use crate::value::PrimitiveKind;
use std::collections::BTreeMap;

/// A validated service model. Build one with [`crate::ModelBuilder`].
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub namespace: String,
    pub container: String,
    pub entity_types: BTreeMap<String, EntityType>,
    pub complex_types: BTreeMap<String, ComplexType>,
    pub enum_types: BTreeMap<String, EnumType>,
    pub entity_sets: BTreeMap<String, EntitySet>,
    pub singletons: BTreeMap<String, Singleton>,
    pub functions: BTreeMap<String, Vec<Operation>>,
    pub actions: BTreeMap<String, Vec<Operation>>,
    pub annotations: Vec<Annotation>,
}

impl Model {
    pub fn qualified(&self, name: &str) -> String {
        format!("{}.{name}", self.namespace)
    }

    /// Strips the service namespace from a qualified name.
    pub fn local<'a>(&self, name: &'a str) -> &'a str {
        name.strip_prefix(self.namespace.as_str())
            .and_then(|r| r.strip_prefix('.'))
            .unwrap_or(name)
    }

    pub fn entity_type(&self, name: &str) -> Option<&EntityType> {
        self.entity_types.get(self.local(name))
    }

    pub fn complex_type(&self, name: &str) -> Option<&ComplexType> {
        self.complex_types.get(self.local(name))
    }

    pub fn enum_type(&self, name: &str) -> Option<&EnumType> {
        self.enum_types.get(self.local(name))
    }

    pub fn entity_set_of_type(&self, type_name: &str) -> Option<&EntitySet> {
        self.entity_sets
            .values()
            .find(|s| s.entity_type == type_name)
    }

    /// `name` followed by each base type, nearest first.
    pub fn type_chain(&self, name: &str) -> Vec<&EntityType> {
        let mut out = Vec::new();
        let mut next = self.entity_type(name);
        while let Some(t) = next {
            if out.iter().any(|seen: &&EntityType| seen.name == t.name) {
                break;
            }
            next = t.base_type.as_deref().and_then(|b| self.entity_type(b));
            out.push(t);
        }
        out
    }

    /// Own and inherited properties, base type first, as they appear on the wire.
    pub fn all_properties(&self, type_name: &str) -> Vec<&Property> {
        let mut chain = self.type_chain(type_name);
        chain.reverse();
        chain.iter().flat_map(|t| t.properties.iter()).collect()
    }

    pub fn all_navigation(&self, type_name: &str) -> Vec<&NavigationProperty> {
        let mut chain = self.type_chain(type_name);
        chain.reverse();
        chain.iter().flat_map(|t| t.navigation.iter()).collect()
    }

    pub fn property(&self, type_name: &str, name: &str) -> Option<&Property> {
        self.type_chain(type_name)
            .into_iter()
            .find_map(|t| t.properties.iter().find(|p| p.name == name))
    }

    pub fn navigation(&self, type_name: &str, name: &str) -> Option<&NavigationProperty> {
        self.type_chain(type_name)
            .into_iter()
            .find_map(|t| t.navigation.iter().find(|n| n.name == name))
    }

    /// The key of `type_name`, which may be declared on a base type.
    pub fn key_of(&self, type_name: &str) -> &[String] {
        self.type_chain(type_name)
            .last()
            .map(|t| t.key.as_slice())
            .unwrap_or(&[])
    }

    pub fn etag_property(&self, type_name: &str) -> Option<&Property> {
        self.type_chain(type_name).into_iter().find_map(|t| {
            t.etag
                .as_deref()
                .and_then(|e| t.properties.iter().find(|p| p.name == e))
        })
    }

    /// Derived types of `name`, transitively.
    pub fn derived_types(&self, name: &str) -> Vec<&EntityType> {
        let mut out = Vec::new();
        for t in self.entity_types.values() {
            if t.name != name && self.type_chain(&t.name).iter().any(|b| b.name == name) {
                out.push(t);
            }
        }
        out
    }
}

#[derive(Clone, Debug, Default)]
pub struct EntityType {
    pub name: String,
    pub table: String,
    pub key: Vec<String>,
    pub properties: Vec<Property>,
    pub navigation: Vec<NavigationProperty>,
    pub base_type: Option<String>,
    pub is_abstract: bool,
    pub inheritance: Option<Inheritance>,
    pub etag: Option<String>,
    pub media: Option<MediaStream>,
    pub visible_for: Vec<String>,
    pub annotations: Vec<Annotation>,
    /// Filter restricting every read, registered with a query extension provider.
    pub query_extension: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inheritance {
    /// Base and derived share a table, told apart by a discriminator.
    SingleTable {
        discriminator: String,
        value: String,
    },
    /// The derived type adds a table joined to the base table.
    Joined {
        join: Vec<ColumnPair>,
        discriminator: Option<(String, String)>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaStream {
    pub content_column: String,
    pub content_type: String,
    pub content_type_column: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ComplexType {
    pub name: String,
    pub properties: Vec<Property>,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug, Default)]
pub struct EnumType {
    pub name: String,
    pub is_flags: bool,
    pub stored_as_string: bool,
    pub members: Vec<(String, i64)>,
}

impl EnumType {
    pub fn member_value(&self, name: &str) -> Option<i64> {
        self.members
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
    }

    pub fn member_name(&self, value: i64) -> Option<&str> {
        self.members
            .iter()
            .find(|(_, v)| *v == value)
            .map(|(n, _)| n.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Primitive(PrimitiveKind),
    Enum(String),
    Complex(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facets {
    pub max_length: Option<u32>,
    pub precision: Option<u32>,
    pub scale: Option<u32>,
    pub srid: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Property {
    pub name: String,
    /// Column of a scalar, or the column prefix of an embedded complex type.
    pub column: String,
    pub kind: TypeKind,
    pub nullable: bool,
    pub facets: Facets,
    pub collection: Option<CollectionTable>,
    pub searchable: bool,
    /// Generated by the database or the server. Ignored in write payloads.
    pub computed: bool,
    /// Calculated by a registered transient calculator rather than stored.
    pub transient: Option<Transient>,
    pub description: Option<DescriptionMapping>,
    pub protections: Vec<Protection>,
    pub visible_for: Vec<String>,
    pub annotations: Vec<Annotation>,
}

impl Property {
    pub fn new(name: impl Into<String>, column: impl Into<String>, kind: TypeKind) -> Self {
        Self {
            name: name.into(),
            column: column.into(),
            kind,
            nullable: true,
            facets: Facets::default(),
            collection: None,
            searchable: false,
            computed: false,
            transient: None,
            description: None,
            protections: Vec::new(),
            visible_for: Vec::new(),
            annotations: Vec::new(),
        }
    }

    pub fn is_stored_scalar(&self) -> bool {
        self.collection.is_none()
            && self.transient.is_none()
            && self.description.is_none()
            && !matches!(self.kind, TypeKind::Complex(_))
    }

    pub fn type_name(&self, model: &Model) -> String {
        match &self.kind {
            TypeKind::Primitive(p) => p.edm_name().to_owned(),
            TypeKind::Enum(n) | TypeKind::Complex(n) => model.qualified(n),
        }
    }
}

/// A pair of columns joined by equality, in source then target order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnPair {
    pub source: String,
    pub target: String,
}

impl ColumnPair {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }
}

/// Storage of a collection property, always a child table keyed by the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionTable {
    pub table: String,
    pub owner: Vec<ColumnPair>,
    /// Value column of a primitive collection. Complex collections map their own properties.
    pub value_column: Option<String>,
    pub order_column: Option<String>,
}

/// A property calculated in application code from other properties.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transient {
    pub requires: Vec<String>,
}

/// A language-dependent text resolved from a description table, like `EdmDescriptionAssociation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptionMapping {
    pub table: String,
    pub join: Vec<ColumnPair>,
    pub language_column: String,
    pub text_column: String,
}

/// A row-level restriction: the column must match the values of a claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protection {
    pub claim: String,
    /// Claim values may contain the wildcards `*` and `+` (or `%` and `_`).
    pub allow_wildcards: bool,
}

#[derive(Clone, Debug, Default)]
pub struct NavigationProperty {
    pub name: String,
    pub target: String,
    pub collection: bool,
    pub nullable: bool,
    /// Direct join, in source then target column order. Empty when `join_table` is used.
    pub join: Vec<ColumnPair>,
    pub join_table: Option<JoinTable>,
    pub partner: Option<String>,
    pub contains_target: bool,
    pub visible_for: Vec<String>,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinTable {
    pub table: String,
    /// Source table column to join table column.
    pub source: Vec<ColumnPair>,
    /// Join table column to target table column.
    pub target: Vec<ColumnPair>,
}

#[derive(Clone, Debug, Default)]
pub struct EntitySet {
    pub name: String,
    pub entity_type: String,
    pub include_in_service_document: bool,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug, Default)]
pub struct Singleton {
    pub name: String,
    pub entity_type: String,
    pub annotations: Vec<Annotation>,
}

/// A function or action overload.
#[derive(Clone, Debug, Default)]
pub struct Operation {
    pub name: String,
    pub is_bound: bool,
    pub is_composable: bool,
    /// Bound operations list the binding parameter first.
    pub parameters: Vec<Parameter>,
    pub return_type: Option<OperationType>,
    /// Database function called by SQL. `None` means the handler registered under `name` runs.
    pub database_function: Option<String>,
    pub visible_for: Vec<String>,
    pub annotations: Vec<Annotation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OperationType {
    pub kind: OperationTypeKind,
    pub collection: bool,
    pub nullable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationTypeKind {
    Value(TypeKind),
    Entity(String),
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub name: String,
    pub ty: OperationType,
}

impl OperationType {
    pub fn type_name(&self, model: &Model) -> String {
        match &self.kind {
            OperationTypeKind::Value(TypeKind::Primitive(p)) => p.edm_name().to_owned(),
            OperationTypeKind::Value(TypeKind::Enum(n) | TypeKind::Complex(n))
            | OperationTypeKind::Entity(n) => model.qualified(n),
        }
    }
}

/// A term applied to a model element.
#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    /// Qualified term, either full (`Org.OData.Core.V1.Description`) or by alias (`Core.Description`).
    pub term: String,
    pub value: AnnotationValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AnnotationValue {
    Bool(bool),
    Int(i64),
    String(String),
    /// A member of a vocabulary enum, such as `Core.Permission/Read`.
    EnumMember(String),
    Path(String),
    Collection(Vec<AnnotationValue>),
    Record(Vec<(String, AnnotationValue)>),
}

impl Annotation {
    pub fn new(term: impl Into<String>, value: AnnotationValue) -> Self {
        Self {
            term: term.into(),
            value,
        }
    }
}

/// The standard OASIS vocabularies, by alias. Referenced from `$metadata` when used.
pub const VOCABULARIES: [(&str, &str, &str); 5] = [
    (
        "Core",
        "Org.OData.Core.V1",
        "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/Org.OData.Core.V1.xml",
    ),
    (
        "Capabilities",
        "Org.OData.Capabilities.V1",
        "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/Org.OData.Capabilities.V1.xml",
    ),
    (
        "Measures",
        "Org.OData.Measures.V1",
        "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/Org.OData.Measures.V1.xml",
    ),
    (
        "Validation",
        "Org.OData.Validation.V1",
        "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/Org.OData.Validation.V1.xml",
    ),
    (
        "Authorization",
        "Org.OData.Authorization.V1",
        "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/Org.OData.Authorization.V1.xml",
    ),
];
