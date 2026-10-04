//! Compaction: overlay merge of the diff chain.
//!
//! Per docs/RELIABILITY-REBUILD.md §4.4.4. Diffs are sparse page maps; the
//! merge is **page overlay by offset, newest wins** — not byte
//! concatenation. The RBD head and the manifest table are NEVER touched
//! here; this is purely about the mem-snap diffs.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// One page in a memory snapshot. Offset is guest-physical; the page is
/// 4 KiB. A diff holds only dirtied pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub offset: u64,
    pub data: Vec<u8>,
}

impl Page {
    pub fn new(offset: u64, data: Vec<u8>) -> Self {
        Self { offset, data }
    }
}

/// Trigger thresholds (RSD §4.4.4).
pub const MERGE_DEPTH: usize = 8;
pub const MERGE_RAM_RATIO_NUM: u64 = 1;
pub const MERGE_RAM_RATIO_DEN: u64 = 2; // 0.5 * RAM

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// Below the threshold; do nothing.
    None,
    /// Chain depth reached MERGE_DEPTH.
    Depth,
    /// Sum of diff sizes since the last base exceeded RAM × 0.5.
    SizeLimit,
}

pub fn should_compact(chain_depth: usize, diff_bytes_sum: u64, ram_bytes: u64) -> CompactionTrigger {
    if chain_depth >= MERGE_DEPTH {
        return CompactionTrigger::Depth;
    }
    let limit = (ram_bytes * MERGE_RAM_RATIO_NUM) / MERGE_RAM_RATIO_DEN;
    if diff_bytes_sum >= limit {
        return CompactionTrigger::SizeLimit;
    }
    CompactionTrigger::None
}

/// Overlay merge: pages from the latest manifest win on conflict.
/// Returns the merged page list in ascending offset order.
///
/// `pages[0]` is the OLDEST layer (the current full base); `pages[len-1]`
/// is the NEWEST layer (the most recent diff). Newer layers overwrite
/// older ones on offset collision.
pub fn overlay_merge(pages: &[Vec<Page>]) -> Vec<Page> {
    use std::collections::BTreeMap;
    let mut out: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    // Walk oldest -> newest, so newer writes overwrite older ones.
    for layer in pages.iter() {
        for p in layer {
            out.insert(p.offset, p.data.clone());
        }
    }
    out.into_iter()
        .map(|(offset, data)| Page::new(offset, data))
        .collect()
}

/// A planned compaction (RSD §4.4.4 step 2): which manifest to anchor at,
/// and what the new full base would be.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    pub session_id: Uuid,
    pub anchor_manifest_id: Uuid,
    pub trigger: CompactionTrigger,
    pub ts: DateTime<Utc>,
    pub merged_pages: Vec<Page>,
}

pub struct OverlayMerger;

impl OverlayMerger {
    pub fn new() -> Self {
        Self
    }

    /// Plan a compaction. The caller is responsible for actually writing
    /// the merged bytes (Phase 3 captures it through the normal path) and
    /// then re-parenting (RSD §4.4.4 step 5).
    pub fn plan(
        &self,
        session_id: Uuid,
        anchor_manifest_id: Uuid,
        trigger: CompactionTrigger,
        layers: &[Vec<Page>],
    ) -> CompactionPlan {
        CompactionPlan {
            session_id,
            anchor_manifest_id,
            trigger,
            ts: Utc::now(),
            merged_pages: overlay_merge(layers),
        }
    }
}

impl Default for OverlayMerger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_depth() {
        assert_eq!(should_compact(8, 0, 0), CompactionTrigger::Depth);
    }

    #[test]
    fn trigger_size() {
        // 100 > 0.5 * 100
        assert_eq!(should_compact(2, 100, 100), CompactionTrigger::SizeLimit);
    }

    #[test]
    fn trigger_none() {
        assert_eq!(should_compact(0, 0, 1024), CompactionTrigger::None);
    }

    #[test]
    fn overlay_newest_wins() {
        let p0 = vec![Page::new(0, vec![1, 2, 3]), Page::new(4096, vec![10])];
        let p1 = vec![Page::new(0, vec![9, 9, 9])]; // overwrites
        let merged = overlay_merge(&[p0, p1]);
        assert_eq!(merged.len(), 2);
        // Newest is layer index 1, so its value for offset 0 wins.
        assert_eq!(merged[0].offset, 0);
        assert_eq!(merged[0].data, vec![9, 9, 9]);
    }

    #[test]
    fn overlay_preserves_unique_pages() {
        let p0 = vec![Page::new(0, vec![1])];
        let p1 = vec![Page::new(4096, vec![2])];
        let merged = overlay_merge(&[p0, p1]);
        assert_eq!(merged.len(), 2);
    }
}