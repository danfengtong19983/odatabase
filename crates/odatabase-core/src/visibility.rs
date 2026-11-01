//! Per-user views of the model, like `EdmVisibleFor` on the Java side.

use odatabase_edm::{ComplexType, Model};
use odatabase_sql::Claims;

fn is_restricted(m: &Model) -> bool {
    m.entity_types.values().any(|t| {
        !t.visible_for.is_empty()
            || t.properties.iter().any(|p| !p.visible_for.is_empty())
            || t.navigation.iter().any(|n| !n.visible_for.is_empty())
    }) || m.complex_types.values().any(has_restricted_property)
        || m.functions
            .values()
            .chain(m.actions.values())
            .flatten()
            .any(|o| !o.visible_for.is_empty())
}

fn has_restricted_property(c: &ComplexType) -> bool {
    c.properties.iter().any(|p| !p.visible_for.is_empty())
}

/// The model as `claims` may see it, or `None` when nothing is hidden from them.
pub fn visible_model(model: &Model, claims: &Claims) -> Option<Model> {
    if !is_restricted(model) {
        return None;
    }
    let mut m = model.clone();
    m.entity_types.retain(|_, t| claims.sees(&t.visible_for));
    let types: Vec<String> = m.entity_types.keys().cloned().collect();
    for t in m.entity_types.values_mut() {
        t.properties.retain(|p| claims.sees(&p.visible_for));
        t.navigation
            .retain(|n| claims.sees(&n.visible_for) && types.contains(&n.target));
    }
    for c in m.complex_types.values_mut() {
        c.properties.retain(|p| claims.sees(&p.visible_for));
    }
    m.entity_sets.retain(|_, s| types.contains(&s.entity_type));
    m.singletons.retain(|_, s| types.contains(&s.entity_type));
    for ops in m.functions.values_mut().chain(m.actions.values_mut()) {
        ops.retain(|o| claims.sees(&o.visible_for));
    }
    m.functions.retain(|_, ops| !ops.is_empty());
    m.actions.retain(|_, ops| !ops.is_empty());
    Some(m)
}
