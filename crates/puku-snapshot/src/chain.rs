//! Diff-chain bookkeeping: current base + diffs after it.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.4.4. The chain has a base (full)
//! followed by N diffs. New captures extend the chain; compaction replaces
//! the base and re-parents the in-flight captures (the cut-over is in
//! `compaction.rs`).

use uuid::Uuid;

use crate::manifest::Manifest;

#[derive(Debug, Clone)]
pub struct ChainState {
    pub base: Manifest,
    pub diffs: Vec<Manifest>,
}

impl ChainState {
    pub fn depth(&self) -> usize {
        // base + diffs
        1 + self.diffs.len()
    }

    pub fn sum_diff_bytes(&self) -> u64 {
        self.diffs.iter().map(|d| d.mem_snap_size_bytes).sum()
    }

    pub fn head(&self) -> &Manifest {
        self.diffs.last().unwrap_or(&self.base)
    }

    pub fn contains_durable(&self) -> bool {
        self.base.status.is_durable()
            || self.diffs.iter().any(|m| m.status.is_durable())
    }
}

/// Walk the parent chain starting at `start` and return the manifests in
/// order base → ... → start. Each manifest's `parent_manifest_id` MUST
/// point at the previous one. The chain ends at the first full snapshot.
pub fn walk_to_base(start: &Manifest, by_id: impl Fn(Uuid) -> Option<Manifest>) -> Vec<Manifest> {
    let mut out = vec![start.clone()];
    let mut cur = start.clone();
    while let Some(parent_id) = cur.parent_manifest_id {
        match by_id(parent_id) {
            Some(p) => {
                if out.iter().any(|m| m.id == p.id) {
                    // Cycle — give up rather than loop forever.
                    break;
                }
                out.push(p.clone());
                cur = p;
            }
            None => break,
        }
    }
    out.reverse();
    out
}

/// Group a flat list of manifests into ChainState (one per session_id).
pub fn group_into_chains(manifests: &[Manifest]) -> Vec<ChainState> {
    use std::collections::BTreeMap;
    let mut by_session: BTreeMap<Uuid, Vec<Manifest>> = BTreeMap::new();
    for m in manifests {
        by_session.entry(m.session_id).or_default().push(m.clone());
    }
    let mut chains = Vec::new();
    for (_sid, mut ms) in by_session {
        ms.sort_by_key(|m| m.ts);
        let base = match ms.iter().find(|m| m.is_chain_base()) {
            Some(b) => b.clone(),
            None => continue, // no base; skip
        };
        let base_idx = ms.iter().position(|m| m.id == base.id).unwrap();
        let diffs: Vec<Manifest> = ms[base_idx + 1..].to_vec();
        chains.push(ChainState { base, diffs });
    }
    chains
}

pub trait IsDurable {
    fn is_durable(&self) -> bool;
}
impl IsDurable for crate::manifest::SnapshotStatus {
    fn is_durable(&self) -> bool {
        matches!(
            self,
            crate::manifest::SnapshotStatus::LocalDurable
                | crate::manifest::SnapshotStatus::Durable
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::SnapshotStatus;
    use chrono::Utc;

    fn make(parent: Option<Uuid>, ts_offset: i64, id: Uuid) -> Manifest {
        let mut m = Manifest::new_pending(
            Uuid::new_v4(),
            "snap",
            "mem",
            1,
            parent.is_none(),
            parent,
            Uuid::new_v4(),
            vec![],
            "kvm",
            "h",
        );
        m.id = id;
        m.ts = Utc::now() + chrono::Duration::seconds(ts_offset);
        m.status = SnapshotStatus::LocalDurable;
        m
    }

    /// Same as `make` but force a session_id for the chain-grouping tests.
    fn make_for_session(parent: Option<Uuid>, ts_offset: i64, id: Uuid, sid: Uuid) -> Manifest {
        let mut m = make(parent, ts_offset, id);
        m.session_id = sid;
        m
    }

    #[test]
    fn walk_to_base_returns_full_chain() {
        let base_id = Uuid::new_v4();
        let d1_id = Uuid::new_v4();
        let d2_id = Uuid::new_v4();
        let base = make(None, 0, base_id);
        let d1 = make(Some(base_id), 1, d1_id);
        let d2 = make(Some(d1_id), 2, d2_id);
        let by_id = |id: Uuid| -> Option<Manifest> {
            if id == base_id {
                Some(base.clone())
            } else if id == d1_id {
                Some(d1.clone())
            } else if id == d2_id {
                Some(d2.clone())
            } else {
                None
            }
        };
        let chain = walk_to_base(&d2, by_id);
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, base_id);
        assert_eq!(chain[2].id, d2_id);
    }

    #[test]
    fn group_chains_picks_a_full_base() {
        // Build a simple chain: base (full) + diff. Same session_id so
        // they end up in the same bucket.
        let sid = Uuid::new_v4();
        let base = make_for_session(None, 0, Uuid::new_v4(), sid);
        let d1 = make_for_session(Some(base.id), 1, Uuid::new_v4(), sid);
        let ms = vec![base.clone(), d1.clone()];
        let chains = group_into_chains(&ms);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth(), 2);
        assert_eq!(chains[0].base.id, base.id);
    }

    #[test]
    fn group_chains_skips_orphans() {
        // Orphan diff with no base in the list should not produce a chain
        // -- we cannot resolve its base.
        let d1 = make(Some(Uuid::new_v4()), 1, Uuid::new_v4());
        let chains = group_into_chains(&[d1]);
        assert!(chains.is_empty());
    }
}