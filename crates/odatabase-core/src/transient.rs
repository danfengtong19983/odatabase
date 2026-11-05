//! Properties calculated in application code after a read.

use crate::service::Inner;
use odatabase_edm::Value;
use odatabase_sql::{Node, Record};

/// The stored values a transient property declared as its inputs.
pub struct TransientRow<'r> {
    record: &'r Record,
}

impl TransientRow<'_> {
    /// An input property's value by name.
    pub fn value(&self, property: &str) -> Option<&Value> {
        self.record
            .value(property)
            .or_else(|| self.record.hidden.get(&format!("transient:{property}")))
    }
}

/// Computes a transient property, like `EdmTransientPropertyCalculator`.
pub trait TransientCalculator: Send + Sync {
    /// The property's value for one row.
    fn calculate(&self, row: &TransientRow<'_>) -> Value;
}

/// Fills the transient placeholders of `records` and of what they expand.
pub(crate) fn apply(inner: &Inner, records: &mut [Record]) {
    if inner.calculators.is_empty() {
        return;
    }
    for rec in records {
        for (name, node) in std::mem::take(&mut rec.fields) {
            let calculated = inner
                .model
                .type_chain(&rec.type_name)
                .iter()
                .find_map(|t| inner.calculators.get(&(t.name.clone(), name.clone())))
                .map(|c| c.calculate(&TransientRow { record: rec }));
            rec.fields.push((name, node));
            if let (Some(v), Some((_, slot))) = (calculated, rec.fields.last_mut()) {
                *slot = Node::Value(v);
            }
        }
        for (_, node) in &mut rec.fields {
            match node {
                Node::Entity(Some(r)) => apply(inner, std::slice::from_mut(r.as_mut())),
                Node::Entities { items, .. } => apply(inner, items),
                _ => {}
            }
        }
    }
}
