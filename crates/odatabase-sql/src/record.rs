use odatabase_edm::Value;
use std::collections::BTreeMap;

/// An entity or complex value read from the database.
#[doc(hidden)]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    /// Concrete type. Differs from the requested type for instances of derived types.
    pub type_name: String,
    pub fields: Vec<(String, Node)>,
    /// Columns loaded for joining but not requested by the client.
    pub hidden: BTreeMap<String, Value>,
}

impl Record {
    pub fn get(&self, name: &str) -> Option<&Node> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Node> {
        self.fields
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    pub fn value(&self, name: &str) -> Option<&Value> {
        match self.get(name) {
            Some(Node::Value(v)) => Some(v),
            _ => None,
        }
    }

    pub fn set(&mut self, name: impl Into<String>, node: Node) {
        let name = name.into();
        match self.get_mut(&name) {
            Some(slot) => *slot = node,
            None => self.fields.push((name, node)),
        }
    }

    /// Writes a value at a property path, creating complex records along the way.
    pub fn set_path(&mut self, path: &[String], value: Value) {
        match path {
            [] => {}
            [name] => self.set(name.clone(), Node::Value(value)),
            [name, rest @ ..] => {
                if !matches!(self.get(name), Some(Node::Complex(Some(_)))) {
                    self.set(name.clone(), Node::Complex(Some(Record::default())));
                }
                if let Some(Node::Complex(Some(inner))) = self.get_mut(name) {
                    inner.set_path(rest, value);
                }
            }
        }
    }

    /// Replaces complex values whose properties are all null with a null value.
    pub fn collapse_empty_complex(&mut self) {
        for (_, node) in &mut self.fields {
            if let Node::Complex(slot) = node {
                if let Some(inner) = slot {
                    inner.collapse_empty_complex();
                }
                let empty = slot.as_ref().is_some_and(|r| {
                    r.fields
                        .iter()
                        .all(|(_, n)| matches!(n, Node::Value(Value::Null) | Node::Complex(None)))
                });
                if empty {
                    *slot = None;
                }
            }
        }
    }
}

/// A property value of a [`Record`].
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Value(Value),
    Complex(Option<Record>),
    /// Collection property.
    Collection(Vec<Node>),
    /// Expanded to-one navigation.
    Entity(Option<Box<Record>>),
    /// Expanded to-many navigation.
    Entities {
        items: Vec<Record>,
        count: Option<i64>,
        next_link: Option<String>,
    },
    /// `$expand=Nav/$count`.
    Count(i64),
    /// `$expand=Nav/$ref`.
    Refs(Refs),
}

/// The target keys of `$expand=Nav/$ref`.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct Refs {
    pub collection: bool,
    pub items: Vec<Record>,
}
