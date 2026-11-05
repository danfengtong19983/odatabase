//! The service model: everything `$metadata` publishes, plus the database mapping behind it.

use crate::value::PrimitiveKind;
use std::collections::BTreeMap;

/// A validated service model. Build one with [`crate::ModelBuilder`].
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// Schema namespace that qualifies every type name.
    pub namespace: String,
    /// Name of the entity container.
    pub container: String,
    /// Entity types by unqualified name.
    pub entity_types: BTreeMap<String, EntityType>,
    /// Complex types by unqualified name.
    pub complex_types: BTreeMap<String, ComplexType>,
    /// Enum types by unqualified name.
    pub enum_types: BTreeMap<String, EnumType>,
    /// Entity sets by name.
    pub entity_sets: BTreeMap<String, EntitySet>,
    /// Singletons by name.
    pub singletons: BTreeMap<String, Singleton>,
    /// Function overloads by name.
    pub functions: BTreeMap<String, Vec<Operation>>,
    /// Action overloads by name.
    pub actions: BTreeMap<String, Vec<Operation>>,
    /// Annotations on the entity container.
    pub annotations: Vec<Annotation>,
}

impl Model {
    /// `name` qualified with the service namespace.
    pub fn qualified(&self, name: &str) -> String {
        format!("{}.{name}", self.namespace)
    }

    /// Strips the service namespace from a qualified name.
    pub fn local<'a>(&self, name: &'a str) -> &'a str {
        name.strip_prefix(self.namespace.as_str())
            .and_then(|r| r.strip_prefix('.'))
            .unwrap_or(name)
    }

    /// An entity type by qualified or unqualified name.
    pub fn entity_type(&self, name: &str) -> Option<&EntityType> {
        self.entity_types.get(self.local(name))
    }

    /// A complex type by qualified or unqualified name.
    pub fn complex_type(&self, name: &str) -> Option<&ComplexType> {
        self.complex_types.get(self.local(name))
    }

    /// An enum type by qualified or unqualified name.
    pub fn enum_type(&self, name: &str) -> Option<&EnumType> {
        self.enum_types.get(self.local(name))
    }

    /// The first entity set whose type is `type_name`.
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

    /// Own and inherited navigation properties, base type first.
    pub fn all_navigation(&self, type_name: &str) -> Vec<&NavigationProperty> {
        let mut chain = self.type_chain(type_name);
        chain.reverse();
        chain.iter().flat_map(|t| t.navigation.iter()).collect()
    }

    /// A property of `type_name` or of one of its base types.
    pub fn property(&self, type_name: &str, name: &str) -> Option<&Property> {
        self.type_chain(type_name)
            .into_iter()
            .find_map(|t| t.properties.iter().find(|p| p.name == name))
    }

    /// A navigation property of `type_name` or of one of its base types.
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

    /// The tenant column of `type_name`, which may be declared on a base type.
    pub fn tenant_of(&self, type_name: &str) -> Option<&TenantColumn> {
        self.type_chain(type_name)
            .into_iter()
            .find_map(|t| t.tenant.as_ref())
    }

    /// The property whose value is the ETag of `type_name`, if it has one.
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

/// An entity type and the table it is stored in.
#[derive(Clone, Debug, Default)]
pub struct EntityType {
    /// Unqualified name.
    pub name: String,
    /// Database table.
    pub table: String,
    /// Names of the key properties.
    pub key: Vec<String>,
    /// Structural properties declared on this type, without inherited ones.
    pub properties: Vec<Property>,
    /// Navigation properties declared on this type, without inherited ones.
    pub navigation: Vec<NavigationProperty>,
    /// Qualified name of the base type.
    pub base_type: Option<String>,
    /// Whether the type has no instances of its own.
    pub is_abstract: bool,
    /// How a derived type is stored.
    pub inheritance: Option<Inheritance>,
    /// Name of the property whose value is the ETag.
    pub etag: Option<String>,
    /// Storage of the media stream, for media entities.
    pub media: Option<MediaStream>,
    /// Groups that may see the type. Empty means everyone.
    pub visible_for: Vec<String>,
    /// Annotations on the type.
    pub annotations: Vec<Annotation>,
    /// Filter restricting every read, registered with a query extension provider.
    pub query_extension: bool,
    /// Changes go to the change log, so that reads can offer delta links.
    pub track_changes: bool,
    /// The column that says which tenant a row belongs to.
    pub tenant: Option<TenantColumn>,
}

/// The tenant of a row. It is no property: clients can neither see nor name it, and every
/// statement on the table is scoped by the claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantColumn {
    /// Database column.
    pub column: String,
    /// Claim whose values are the tenants of the caller.
    pub claim: String,
    /// Type of the column.
    pub kind: PrimitiveKind,
}

/// How a derived entity type is stored.
#[derive(Clone, Debug, PartialEq)]
pub enum Inheritance {
    /// Base and derived share a table, told apart by a discriminator.
    SingleTable {
        /// Discriminator column.
        discriminator: String,
        /// Value of the discriminator column for this type.
        value: String,
    },
    /// The derived type adds a table joined to the base table.
    Joined {
        /// Base table key column to derived table key column.
        join: Vec<ColumnPair>,
        /// Optional discriminator column and value.
        discriminator: Option<(String, String)>,
    },
}

/// Where the stream of a media entity is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaStream {
    /// Column holding the bytes.
    pub content_column: String,
    /// Media type used when there is no `content_type_column`, `application/octet-stream` by default.
    pub content_type: String,
    /// Column holding the media type of each stream.
    pub content_type_column: Option<String>,
}

/// A complex type, stored in the columns of its owner or in a collection table.
#[derive(Clone, Debug, Default)]
pub struct ComplexType {
    /// Unqualified name.
    pub name: String,
    /// Its properties.
    pub properties: Vec<Property>,
    /// Annotations on the type.
    pub annotations: Vec<Annotation>,
}

/// An enum type.
#[derive(Clone, Debug, Default)]
pub struct EnumType {
    /// Unqualified name.
    pub name: String,
    /// Whether values may combine several members.
    pub is_flags: bool,
    /// Whether the column holds member names rather than numbers.
    pub stored_as_string: bool,
    /// Member names and values.
    pub members: Vec<(String, i64)>,
}

impl EnumType {
    /// The value of a member.
    pub fn member_value(&self, name: &str) -> Option<i64> {
        self.members
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
    }

    /// The member with this value.
    pub fn member_name(&self, value: i64) -> Option<&str> {
        self.members
            .iter()
            .find(|(_, v)| *v == value)
            .map(|(n, _)| n.as_str())
    }
}

/// The type of a structural property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    /// A primitive type.
    Primitive(PrimitiveKind),
    /// An enum type, by unqualified name.
    Enum(String),
    /// A complex type, by unqualified name.
    Complex(String),
}

/// Type facets published in `$metadata` and checked on writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facets {
    /// Maximum length of strings and binaries.
    pub max_length: Option<u32>,
    /// Digits of decimals, or fractional second digits of times.
    pub precision: Option<u32>,
    /// Digits after the decimal point.
    pub scale: Option<u32>,
    /// Spatial reference system of geospatial values.
    pub srid: Option<u32>,
}

/// A structural property and its storage.
#[derive(Clone, Debug)]
pub struct Property {
    /// Name in the model.
    pub name: String,
    /// Column of a scalar, or the column prefix of an embedded complex type.
    pub column: String,
    /// Its type.
    pub kind: TypeKind,
    /// Whether it accepts null.
    pub nullable: bool,
    /// Type facets.
    pub facets: Facets,
    /// Storage of a collection property. `None` for single values.
    pub collection: Option<CollectionTable>,
    /// Whether `$search` looks in it.
    pub searchable: bool,
    /// Generated by the database or the server. Ignored in write payloads.
    pub computed: bool,
    /// Calculated by a registered transient calculator rather than stored.
    pub transient: Option<Transient>,
    /// Language-dependent text read from a description table.
    pub description: Option<DescriptionMapping>,
    /// Claims that restrict which rows a caller sees.
    pub protections: Vec<Protection>,
    /// Groups that may see the property. Empty means everyone.
    pub visible_for: Vec<String>,
    /// Annotations on the property.
    pub annotations: Vec<Annotation>,
}

impl Property {
    /// A nullable, single-valued property stored in `column`.
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

    /// Whether its type is a complex type.
    pub fn is_complex(&self) -> bool {
        matches!(self.kind, TypeKind::Complex(_))
    }

    /// Whether it is a single value stored in a column of the entity table.
    pub fn is_stored_scalar(&self) -> bool {
        self.collection.is_none()
            && self.transient.is_none()
            && self.description.is_none()
            && !matches!(self.kind, TypeKind::Complex(_))
    }

    /// Qualified name of its type.
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
    /// Column on the source side.
    pub source: String,
    /// Column on the target side.
    pub target: String,
}

impl ColumnPair {
    /// A column pair.
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
    /// The child table.
    pub table: String,
    /// Owner column to child table column.
    pub owner: Vec<ColumnPair>,
    /// Value column of a primitive collection. Complex collections map their own properties.
    pub value_column: Option<String>,
    /// Column holding the position in an ordered collection.
    pub order_column: Option<String>,
    /// The tenant column of the table, filled on insert when its owner has a tenant.
    pub tenant_column: Option<String>,
}

/// A property calculated in application code from other properties.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transient {
    /// Properties the calculator reads.
    pub requires: Vec<String>,
}

/// A language-dependent text resolved from a description table, like `EdmDescriptionAssociation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptionMapping {
    /// The description table.
    pub table: String,
    /// Entity column to description table column.
    pub join: Vec<ColumnPair>,
    /// Column holding the language code.
    pub language_column: String,
    /// Column holding the text.
    pub text_column: String,
}

/// A row-level restriction: the column must match the values of a claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protection {
    /// Claim whose values the column must match.
    pub claim: String,
    /// Claim values may contain the wildcards `*` and `+` (or `%` and `_`).
    pub allow_wildcards: bool,
}

/// A navigation property and the join behind it.
#[derive(Clone, Debug, Default)]
pub struct NavigationProperty {
    /// Name in the model.
    pub name: String,
    /// Qualified name of the target type.
    pub target: String,
    /// Whether it leads to many entities.
    pub collection: bool,
    /// Whether a single-valued navigation may be null.
    pub nullable: bool,
    /// Direct join, in source then target column order. Empty when `join_table` is used.
    pub join: Vec<ColumnPair>,
    /// Join through a link table, for many-to-many relationships.
    pub join_table: Option<JoinTable>,
    /// Name of the navigation property that leads back.
    pub partner: Option<String>,
    /// Whether the target entities are contained and addressed only through this property.
    pub contains_target: bool,
    /// Groups that may see the property. Empty means everyone.
    pub visible_for: Vec<String>,
    /// Annotations on the property.
    pub annotations: Vec<Annotation>,
}

/// A link table between the source and target of a navigation property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinTable {
    /// The link table.
    pub table: String,
    /// Source table column to join table column.
    pub source: Vec<ColumnPair>,
    /// Join table column to target table column.
    pub target: Vec<ColumnPair>,
    /// The tenant column of the table, filled on insert when the source has a tenant.
    pub tenant_column: Option<String>,
}

/// An entity set.
#[derive(Clone, Debug, Default)]
pub struct EntitySet {
    /// Name in the URL.
    pub name: String,
    /// Qualified name of the member type.
    pub entity_type: String,
    /// Whether the service document lists it.
    pub include_in_service_document: bool,
    /// Annotations on the set.
    pub annotations: Vec<Annotation>,
}

/// A singleton: one entity addressed by name.
#[derive(Clone, Debug, Default)]
pub struct Singleton {
    /// Name in the URL.
    pub name: String,
    /// Qualified name of its type.
    pub entity_type: String,
    /// Annotations on the singleton.
    pub annotations: Vec<Annotation>,
}

/// A function or action overload.
#[derive(Clone, Debug, Default)]
pub struct Operation {
    /// Unqualified name.
    pub name: String,
    /// Whether it is bound to an entity or collection.
    pub is_bound: bool,
    /// Whether further path segments and query options may follow it.
    pub is_composable: bool,
    /// Bound operations list the binding parameter first.
    pub parameters: Vec<Parameter>,
    /// `None` for an action without a result.
    pub return_type: Option<OperationType>,
    /// Database function called by SQL. `None` means the handler registered under `name` runs.
    pub database_function: Option<String>,
    /// Groups that may call it. Empty means everyone.
    pub visible_for: Vec<String>,
    /// Annotations on the operation.
    pub annotations: Vec<Annotation>,
}

/// The type of a parameter or return value.
#[derive(Clone, Debug, PartialEq)]
pub struct OperationType {
    /// The type.
    pub kind: OperationTypeKind,
    /// Whether it is a collection.
    pub collection: bool,
    /// Whether it accepts null.
    pub nullable: bool,
}

/// The kinds of parameter and return types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationTypeKind {
    /// A primitive, enum or complex type.
    Value(TypeKind),
    /// An entity type, by unqualified name.
    Entity(String),
}

/// An operation parameter.
#[derive(Clone, Debug)]
pub struct Parameter {
    /// Its name.
    pub name: String,
    /// Its type.
    pub ty: OperationType,
}

impl OperationType {
    /// Qualified name of the type.
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
    /// The value.
    pub value: AnnotationValue,
}

/// The value of an annotation.
#[derive(Clone, Debug, PartialEq)]
pub enum AnnotationValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A string.
    String(String),
    /// A member of a vocabulary enum, such as `Core.Permission/Read`.
    EnumMember(String),
    /// A path to a model element.
    Path(String),
    /// A collection.
    Collection(Vec<AnnotationValue>),
    /// A record of named values.
    Record(Vec<(String, AnnotationValue)>),
}

impl Annotation {
    /// An annotation with the term `term`.
    pub fn new(term: impl Into<String>, value: AnnotationValue) -> Self {
        Self {
            term: term.into(),
            value,
        }
    }
}

/// The standard OASIS vocabularies, by alias. Referenced from `$metadata` when used.
pub const VOCABULARIES: [(&str, &str, &str); 6] = [
    (
        "Core",
        "Org.OData.Core.V1",
        "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/Org.OData.Core.V1.xml",
    ),
    (
        "Aggregation",
        "Org.OData.Aggregation.V1",
        "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/Org.OData.Aggregation.V1.xml",
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
