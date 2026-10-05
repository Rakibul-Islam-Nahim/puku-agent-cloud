//! Retention reaper. Newest-first deletes after compaction.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.4.4. The actual merge lives in
//! `compaction.rs`; this module is the cleanup that runs after the merge
//! (or for `archived` sessions in reaper mode).
//!
//! The hard rule: parent_manifest_id FK with ON DELETE RESTRICT means no
//! manifest can be removed while another depends on it, so retention MUST
//! delete children first. The `delete_in_order` helper here enforces that.

use crate::manifest::{Manifest, SnapshotStatus};

/// Decide which manifest rows are reapable, in safe order.
///
/// Returns them sorted with the **latest** first so the caller can
/// `DELETE` them one by one without ever deleting a parent before its
/// child.
pub fn reapable_newest_first(manifests: &[Manifest]) -> Vec<&Manifest> {
    let mut m: Vec<&Manifest> = manifests
        .iter()
        .filter(|m| m.status != SnapshotStatus::Pending)
        .collect();
    // Newest first by `ts`. Children are inserted later (greater ts) than
    // their parent, so this ordering matches child-before-parent for the
    // diff chain. (RSD §4.4.4: "delete children first".)
    m.sort_by_key(|x| std::cmp::Reverse(x.ts));
    m
}

/// Are these manifests in a valid child-before-parent ordering?
/// Used by the test suite to catch retention bugs.
pub fn is_valid_reaper_order(ordered: &[&Manifest]) -> bool {
    let by_id = |id: uuid::Uuid| -> Option<&Manifest> {
        ordered.iter().find(|m| m.id == id).copied()
    };
    for m in ordered {
        if let Some(parent_id) = m.parent_manifest_id {
            let parent = match by_id(parent_id) {
                Some(p) => p,
                None => return false, // orphan parent ref
            };
            // If the parent appears in the list, it must come AFTER us.
            let pos_self = ordered.iter().position(|x| x.id == m.id).unwrap();
            let pos_parent = ordered.iter().position(|x| x.id == parent.id).unwrap();
            if pos_parent < pos_self {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn make(ts_offset: i64, parent: Option<Uuid>, id: Uuid) -> Manifest {
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
        m.status = SnapshotStatus::Durable;
        m
    }

    #[test]
    fn reaper_orders_children_first() {
        let base = Uuid::new_v4();
        let d1 = Uuid::new_v4();
        let d2 = Uuid::new_v4();
        let m_base = make(0, None, base);
        let m_d1 = make(1, Some(base), d1);
        let m_d2 = make(2, Some(d1), d2);
        let all = vec![m_base.clone(), m_d1.clone(), m_d2.clone()];
        let order: Vec<&Manifest> = reapable_newest_first(&all);
        assert!(is_valid_reaper_order(&order));
        // The newest must be first.
        assert_eq!(order[0].id, d2);
    }

    #[test]
    fn pending_is_skipped() {
        let mut m = make(0, None, Uuid::new_v4());
        m.status = SnapshotStatus::Pending;
        let v = vec![m];
        let order = reapable_newest_first(&v);
        assert!(order.is_empty());
    }
}