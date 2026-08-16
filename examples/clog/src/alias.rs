//! Depth-1 entity alias map with write-time flattening (spec §5.2, tests
//! U-ALIAS-1/2).
//!
//! Edges are always depth-1: `insert` flattens the target at write time (if
//! `canonical` is itself aliased, the new edge points at `canonical`'s
//! target instead) and re-points any existing edges that pointed at `alias`
//! so no chain ever forms. `resolve` is therefore always a single hop.

use imbl::OrdMap;

use crate::ClogError;

/// An entity's identity key: `(etype, id)`.
pub(crate) type EntityKey = (String, String);

/// A depth-1 alias map from entity key to its canonical entity key.
#[derive(Clone, Default)]
pub(crate) struct AliasMap {
    edges: OrdMap<EntityKey, EntityKey>,
}

impl AliasMap {
    /// Resolves `k` to its canonical key: one hop, since edges are depth-1
    /// by construction. Identity if `k` has no alias edge.
    pub(crate) fn resolve(&self, k: &EntityKey) -> EntityKey {
        self.edges.get(k).cloned().unwrap_or_else(|| k.clone())
    }

    /// Write-time flattening helper: if `canonical` is itself aliased,
    /// returns its target; otherwise returns `canonical` unchanged.
    pub(crate) fn flatten_target(&self, canonical: &EntityKey) -> EntityKey {
        self.edges.get(canonical).cloned().unwrap_or_else(|| canonical.clone())
    }

    /// Inserts an alias edge `alias -> canonical`, flattening `canonical`
    /// first so edges stay depth-1, then re-pointing any existing edges that
    /// targeted `alias` at the newly flattened canonical. Rejects
    /// `ClogError::AliasCycle` if the flattened target equals `alias`
    /// (covers both reverse edges, e.g. inserting `b -> a` after `a -> b`,
    /// and self-loops, e.g. `z -> z`).
    pub(crate) fn insert(&mut self, alias: EntityKey, canonical: EntityKey) -> Result<(), ClogError> {
        let flattened = self.flatten_target(&canonical);
        if flattened == alias {
            return Err(ClogError::AliasCycle);
        }
        // Re-point any existing edges whose target is `alias` so they keep
        // pointing at a canonical (depth-1), not at `alias` itself.
        let repoint: Vec<EntityKey> = self
            .edges
            .iter()
            .filter(|(_, v)| **v == alias)
            .map(|(k, _)| k.clone())
            .collect();
        for k in repoint {
            self.edges.insert(k, flattened.clone());
        }
        self.edges.insert(alias, flattened);
        Ok(())
    }

    /// Removes any alias edge for `alias`, un-merging it back to its own
    /// identity.
    pub(crate) fn remove(&mut self, alias: &EntityKey) {
        self.edges.remove(alias);
    }

    /// Iterates over the raw alias edges.
    // Not yet called from production code: consumed by the entity-state
    // renderer in a later task. Exercised by this module's tests meanwhile.
    #[allow(dead_code)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&EntityKey, &EntityKey)> {
        self.edges.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn k(e: &str, i: &str) -> EntityKey {
        (e.into(), i.into())
    }

    #[test]
    fn u_alias_1_write_time_flattening() {
        let mut m = AliasMap::default();
        m.insert(k("p", "a"), k("p", "b")).unwrap();
        // b -> c: the stored edge for a must re-point to c (depth-1, no chains)
        m.insert(k("p", "b"), k("p", "c")).unwrap();
        assert_eq!(m.resolve(&k("p", "a")), k("p", "c"));
        assert_eq!(m.resolve(&k("p", "b")), k("p", "c"));
        // inserting x -> a flattens to x -> c at write time
        m.insert(k("p", "x"), k("p", "a")).unwrap();
        assert_eq!(m.resolve(&k("p", "x")), k("p", "c"));
        assert_eq!(m.resolve(&k("p", "unrelated")), k("p", "unrelated"));
    }

    #[test]
    fn u_alias_2_cycle_rejected() {
        let mut m = AliasMap::default();
        m.insert(k("p", "a"), k("p", "b")).unwrap();
        assert!(matches!(m.insert(k("p", "b"), k("p", "a")), Err(crate::ClogError::AliasCycle)));
        assert!(matches!(m.insert(k("p", "z"), k("p", "z")), Err(crate::ClogError::AliasCycle)));
    }

    #[test]
    fn remove_unmerges() {
        let mut m = AliasMap::default();
        m.insert(k("p", "a"), k("p", "b")).unwrap();
        m.remove(&k("p", "a"));
        assert_eq!(m.resolve(&k("p", "a")), k("p", "a"));
    }
}
