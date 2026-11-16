//! Per-user views of the model, like `EdmVisibleFor` on the Java side.

use bytes::Bytes;
use odatabase_edm::{ComplexType, Model};
use odatabase_sql::Claims;
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

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

/// Rendered `$metadata` documents and their ETags, one per format and view of the model.
///
/// A view depends only on which of the model's `visible_for` groups the caller belongs to, so
/// the cache holds at most one entry per combination that actually occurs.
pub struct MetadataCache {
    groups: BTreeSet<String>,
    entries: Mutex<Entries>,
}

/// Document and ETag per format (`true` for JSON) and the caller's relevant groups.
type Entries = HashMap<(bool, Vec<String>), (Bytes, String)>;

/// More combinations than this are rendered on every request rather than cached.
const MAX_ENTRIES: usize = 256;

impl MetadataCache {
    pub fn new(model: &Model) -> Self {
        let mut groups = BTreeSet::new();
        for t in model.entity_types.values() {
            groups.extend(t.visible_for.iter().cloned());
            for p in &t.properties {
                groups.extend(p.visible_for.iter().cloned());
            }
            for n in &t.navigation {
                groups.extend(n.visible_for.iter().cloned());
            }
        }
        for c in model.complex_types.values() {
            for p in &c.properties {
                groups.extend(p.visible_for.iter().cloned());
            }
        }
        for o in model
            .functions
            .values()
            .chain(model.actions.values())
            .flatten()
        {
            groups.extend(o.visible_for.iter().cloned());
        }
        Self {
            groups,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The document and tag for `claims`, rendered by `render` the first time.
    pub fn get(
        &self,
        json: bool,
        claims: &Claims,
        render: impl FnOnce() -> Bytes,
        tag: impl FnOnce(&[u8]) -> String,
    ) -> (Bytes, String) {
        let mut view: Vec<String> = claims
            .groups
            .iter()
            .filter(|g| self.groups.contains(*g))
            .cloned()
            .collect();
        view.sort();
        view.dedup();
        let key = (json, view);
        if let Some(hit) = self.lock().get(&key) {
            return hit.clone();
        }
        let body = render();
        let entry = (body.clone(), tag(&body));
        let mut entries = self.lock();
        if entries.len() < MAX_ENTRIES {
            entries.insert(key, entry.clone());
        }
        entry
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        // A panic while rendering leaves no partial entry behind, so a poisoned lock is usable.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}
