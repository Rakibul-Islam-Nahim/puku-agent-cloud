# Puku Agent Cloud — Reliability Rebuild Plan

> **Scope of this document.** Reliability only. A complete build spec for turning the current repo into a system that meets the "100% recoverable" promise from the Notion design. Scalability, time efficiency, and storage efficiency are explicitly out of scope and addressed in follow-on docs.
>
> **Audience.** The engineer reading this is the one writing the code. Every section is one "what to do" + "how to know it's done" pair.

---

## 0. Goal, promise, and non-goals

### 0.1 The promise

| Property | Guarantee |
|---|---|
| **Disk data (cold resume)** | **Zero loss** of data the guest flushed (`fsync`/`sync`) to its virtual disk. Cold resume = restore the **latest** disk state from the RBD clone (no rollback). Data still in guest page cache or process memory is lost, which is the cost of fsync-less writes, not a reliability bug. **Required configuration:** the virtio-blk device must honor guest flush requests (no `cache=unsafe`), and the RBD client cache must run writethrough until the first flush (`rbd_cache_writethrough_until_flush = true`) or be disabled. With any other setting an acknowledged flush may not be on Ceph OSDs, and this promise is broken regardless of the code. `preflight.sh` checks these settings. |
| **Disk data (warm resume)** | **At most N seconds of disk rollback**, where N = age of the snapshot used. Warm resume = restore **disk + memory from the same paired snapshot**, so the disk is rewound to that instant. This is the cost of preserving running programs. |
| **Conversation context** | Always recoverable. Transcript is permanent; resume reads the same context. |
| **Running programs (RAM)** | At most **N seconds** lost, where N is tier-dependent: **60 s standard**, **5 s premium**. Loss = the window between the last paired snapshot and the crash. Loss is **RAM only**: cold resume, by definition, accepts full RAM loss (the disk stays — disk is fsync-promised, RAM is not, see the row above). Premium's 5 s figure is conditional on the engine supporting diff memory snapshots AND `premium_requires_hot_pool` being satisfied (§4.4.3a); otherwise it degrades to 60 s with a logged `RPOAtRisk` and an alert. |
| **Split-brain** | Impossible. Fencing always completes before any failover. |
| **Recovery RTO** | < 2 s for sandbox-only failure (local tier-1). < 30 s for host loss (remote tier-2). < 15 min for region loss (requires cross-region replication — see F9 in §1). |
| **Disk, whole Ceph pool lost** | Loss ≤ `disk_backup_interval` (default **1 h**) of disk writes, restored from the off-cluster backup in R2. Installed packages and conversation are never lost. Without a backup chain the floor is `Rebuilt` (§1.1). |
| **Recovery, when automated** | Tested in CI on every PR. Not "tested once, hope it works". |

**The mode is chosen by `recovery_mode` on the session row** (§4.1 schema below). Default for new sessions is **`warm_allowed`** (preserve RAM; roll disk back to the snapshot point if needed). The org-level setting is the **ceiling** (per-org `quotas.recovery_mode` ∈ `cold_only | warm_allowed`); a session can be lowered to `cold_only` (zero disk rollback, full RAM loss on any recovery) but never raised above the org ceiling. Per-tier defaulting is automatic: `premium` → `warm_allowed`, `best_effort` → `cold_only`, `standard` → `warm_allowed`. A warm-resume request only succeeds if `recovery_mode = warm_allowed` AND a paired snapshot is fresh enough. Default is warm (preserve running programs); only opt down to `cold_only` if the workload cannot tolerate any disk rollback.

### 0.2 Non-goals (deferred docs)

| Deferred to | Topic |
|---|---|
| `SCALABILITY-REBUILD.md` | **Multi-region** (cross-region Ceph + R2 + Postgres replication, which is what F9's 15-min RTO actually requires), fleet autoscale, warm-pool snapshots for cold start |
| `TIME-EFFICIENCY-REBUILD.md` | Inotify tail, adaptive seq preallocation, WebSocket compression |
| `STORAGE-EFFICIENCY-REBUILD.md` | R2 lifecycle, IA migration, chunked dedup, cost dashboards |

**F9 caveat.** F9 (whole region gone, < 15 min RTO) is listed in §1 as a failure mode this rebuild handles, but the *mechanism* (cross-region replication) is in the deferred `SCALABILITY-REBUILD.md`. Without that follow-on doc, F9's actual RTO is whatever the operator has set up externally (likely hours, not minutes). The reliability rebuild makes F9 *possible*; it does not deliver it on its own.

### 0.3 Core (unchanged)

The following stay as-is. Do not refactor unless a reliability requirement forces it:

| Core component | File | Why it's core |
|---|---|---|
| `puku-cli` headless invocation | `docs/PUKU-CLI-CONTRACT.md` | Pinned contract, re-verified on every CLI bump |
| One microVM per session | `crates/puku-workerd/src/vm/` | Hardware isolation is the security boundary |
| `puku-controld` API surface | `crates/puku-controld/src/api/` | Public contract; additive only |
| `puku-workerd` per-host daemon | `crates/puku-workerd/src/main.rs` | Node-agent lives inside it, not replacing it |
| `session_events` partitioned log | `migrations/0001_init.sql` | Source of truth, append-only |
| R2 for spill + transcripts + mem snapshots | `crates/puku-controld/src/blobstore.rs` | Already correct, durable, cheap |
| Quota / cost / auth | `migrations/0001_init.sql`, `crates/puku-controld/src/auth/` | Public API contract |
| Dashboard | `crates/puku-controld/src/api/mod.rs` (`GET /`) | Public API contract |

---

## 1. Failure modes and how each is handled

Every single failure mode is enumerated below. Each row is a **contract**: the detection, action, and time budget. **If any row is unimplemented, the rebuild is not done.** Where the action depends on `recovery_mode`, both branches are listed.

| # | Failure | Detection | Action | RTO | Test name (in §6) |
|---|---|---|---|---|---|
| F1 | **Sandbox VM crashes, host healthy** | Node agent sees VMM exit + exit code | Tier-1: restart on same host, RBD already mapped. `recovery_mode = warm_allowed` → **RestoreOutcome::Warm** from the latest `LocalDurable` or `Durable` manifest (sha256 re-verified). `recovery_mode = cold_only` → **RestoreOutcome::ColdHead**: disk is the RBD head, no rollback, no package loss. Agent re-reads transcript; in-flight tools re-run. | < 2 s | `kill_vmm` |
| F2 | **Guest kernel hangs / panics (VMM alive)** | vsock heartbeat stops > 3 s while VMM responds | Tier-1: kill VMM, restart. Same warm/cold branch as F1. | < 2 s | `guest_hang` |
| F3 | **Same VM crashes N≥3 times in M=10 min** | Tier-1 counter | Tier-1 then evict: mark session `crash_count ≥ 3`, next crash routes to `RecoveryChoice::Remote` (fence + remote restart), pick another host by fingerprint match | < 30 s | `crash_loop_eviction` |
| F4 | **Host dies (power off, kernel panic)** | Lease expires within 3 s + BMC power=off OR host unreachable | Tier-2: fence (blocklist + BMC), then restart on healthy host. `recovery_mode = warm_allowed` → **RestoreOutcome::Warm** from latest Durable (only Durable is acceptable remote — RPO = `rpo_seconds + p95_durability_lag`). `recovery_mode = cold_only` → **RestoreOutcome::ColdHead**: RBD clone + replay events. | < 30 s | `host_kernel_panic` |
| F5 | **Network partition (host isolated but alive)** | Lease expires, BMC says power ON, peers can't reach | Tier-2: **fence first** (blocklist), then restart elsewhere. Same warm/cold branch as F4. | < 30 s | `network_partition` |
| F6 | **Two hosts think they own the session** | Impossible after fence: blocklisted host gets I/O errors | Not possible — fenced host's writes are blocked at Ceph | — | (covered by F4/F5) |
| F7 | **Latest snapshot corrupt** | SHA-256 mismatch on restore | Walk parent chain to previous good Durable (depth ≤ `MERGE_DEPTH`). If memory is corrupt but disk head is fine → **RestoreOutcome::ColdHead** (no disk rollback; packages and data preserved). If all durables corrupt → **RestoreOutcome::ColdHead** (current RBD head still works; we never roll the disk back to a snapshot). Volume lost → **RestoreOutcome::FromArchive**. Archive lost → **RestoreOutcome::Rebuilt**. Last resort → **RestoreOutcome::Failed** and page. | < 5 s | `corrupt_snapshot` |
| F8 | **Memory lost** | `recovery_mode = warm_allowed` succeeds → RPO = snapshot age. `recovery_mode = cold_only` → always accept the loss; disk + transcript are durable. | Warm: restart programs from snapshot; agent re-reads transcript for context. Cold: same, plus the agent re-runs any in-flight tool from scratch (T7 explains the transcript annotation). | RPO = `rpo_seconds` per tier (5 / 60 / undefined for `cold_only`) | `warm_resume_after_kill` (warm path), `cold_resume_after_kill` (cold path) |
| F9 | **Whole region gone** | Multiple leases expired + R2 unavailable + Postgres replica unreachable | Bring up new region: RBD from replicated Ceph + R2 from cross-region replication + Postgres replica + recent events from event log | < 15 min (requires cross-region replication — see §0.2) | `region_loss_drill` (manual) |
| F10 | **User data silently deleted** | Never. Deletion requires explicit request and audit. | — | — | `no_silent_delete_invariant` |

### 1.1 The restore ladder (used by every failure mode)

Every recovery uses the **same ladder**, in this order. Earlier rows preserve more; later rows are the floor. The row chosen is returned as a `RestoreOutcome` (§4.4.1).

| # | Outcome | When it is chosen | What is preserved |
|---|---|---|---|
| 1 | `Warm(manifest)` | `LocalDurable` or `Durable` manifest exists and engine compatible | Disk + memory from same instant. RAM RPO = `rpo_seconds` (+ ~0.2 s for `LocalDurable`). |
| 2 | `ColdHead(volume)` | Memory missing, corrupt, or engine-incompatible (diff unsupported, cpu mismatch) | Disk is the **current RBD head** — no rollback. Packages, files, all user data preserved. Full RAM loss; in-flight tools re-run from transcript. |
| 3 | `FromArchive(manifest)` | Volume lost (RBD pool unavailable, image corrupt or gone) | Disk from the best off-cluster copy: the R2 disk archive for `archived` sessions, otherwise the latest **disk backup chain** (`rbd import` full + `rbd import-diff` increments, sha256 verified). Loss = writes since the last backup (≤ `disk_backup_interval`). Memory not available; full RAM loss. |
| 4 | `Rebuilt(volume)` | Disk archive also gone | Fresh base image + replay `installed_packages` + replay event log from `last_seq`. Environment and conversation preserved; data files (other than user text in the transcript) lost. |
| 5 | `Failed(reason)` | None of the above works | Loud failure, operator paged. |

**The disk is never rolled back to a snapshot in `recovery_mode = cold_only`.** Only `warm_allowed` may roll disk + memory together; cold resumes always use the current RBD head or a fresh archive. Cold resume therefore loses RAM but never loses installed packages or user files.

---

## 2. New components (6 new crates, everything else is extension)

### 2.1 Component map

| Crate | Path | Purpose | Replaces |
|---|---|---|---|
| `puku-volume` | `crates/puku-volume/` | Volume abstraction (RBD + Local), fence | Direct RBD calls scattered in `puku-workerd` |
| `puku-leases` | `crates/puku-leases/` | 1–2 s heartbeat + expiry sweeper | None (new responsibility) |
| `puku-fence` | `crates/puku-fence/` | Ceph blocklist + IPMI/Redfish | None (new responsibility) |
| `puku-snapshot` | `crates/puku-snapshot/` | Co-issued disk+mem snapshot, manifest, chain collapse, test-restore | Partial: existing `crates/puku-workerd/src/snapshot/` becomes library inside this crate |
| `puku-proxy` | `crates/puku-proxy/` | Per-session WS proxy with `reconnect_token`; absorbs controld failover so the client never sees a dropped connection mid-`waiting_input` | None (new responsibility) |
| `puku-rebuild` | `crates/puku-rebuild/` | CLI that emits a shell script replaying `installed_packages`, for environment rebuild after total disk loss | None (new responsibility) |

### 2.2 Existing crates extended

| Crate | Path | Extension |
|---|---|---|
| `puku-cloud-proto` | `crates/puku-cloud-proto/` | Add v2 wire types for: `LeaseHeartbeat`, `FenceRequest`, `VolumeAttach`, `SnapshotManifest`, `PairedSnapshot` |
| `puku-controld` | `crates/puku-controld/` | Add modules: `recovery.rs`, `leases.rs`, `fence.rs`, `sweeper.rs`. Extend `scheduler.rs` (fingerprint placement), `archive.rs` (tier transitions), `api/mod.rs` (new states) |
| `puku-workerd` | `crates/puku-workerd/` | Replace local volume access with `puku-volume::RbdBackend`. Add `lease_renew` task. Extend `session_actor.rs` (idle-triggered snapshot, `desired_state` check). Add `fencing_recv` task. |
| `puku-guestd` | `crates/puku-guestd/` | Add `heartbeat.rs` (vsock heartbeat every 1 s), `idle.rs` ("between tool calls" signal), extend `shutdown.rs` (`clean_shutdown` message) |
| `puku-observability` | `crates/puku-observability/` | Add tags: `lease_expired`, `fence_state`, `sla_tier`, `snapshot_manifest_id`. Add metrics for snapshot test-restore and fence outcomes |

---

## 3. Database schema (6 new migrations)

All migrations are **forward-only**; existing rows get defaults.

### 3.1 `migrations/0027_reliability_states.sql`

```sql
-- New session states: recovering, hibernated, cold_archived, archived, deleted
ALTER TABLE sessions DROP CONSTRAINT sessions_state_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_state_check CHECK (state IN (
  'created','scheduled','booting','bootstrapping','running','waiting_input',
  'stopping','stopped','completed','failed','canceled','reaped',
  'recovering','hibernated','cold_archived','archived','deleted'
));

-- Desired vs observed.
-- desired=stopped *before* shutdown → expected shutdown, do nothing.
-- desired=running but observed=missing → crash, recover.
ALTER TABLE sessions ADD COLUMN desired_state text NOT NULL DEFAULT 'running'
  CHECK (desired_state IN ('running','stopped'));

-- Snapshot metadata, populated by puku-snapshot.
ALTER TABLE sessions ADD COLUMN last_snapshot_id text;
ALTER TABLE sessions ADD COLUMN snapshot_taken_at timestamptz;
ALTER TABLE sessions ADD COLUMN snapshot_host_id uuid;
ALTER TABLE sessions ADD COLUMN snapshot_cpu_flags text[] NOT NULL DEFAULT '{}';
ALTER TABLE sessions ADD COLUMN snapshot_hypervisor text;

-- One tier concept lives here. The RPO is derived from sla_tier by a constant
-- (§4.4.3) — there is no separate rpo_target_s column to drift out of sync.
-- NULL rpo means cold_only sessions; their RAM RPO is undefined by design
-- (300 s best-effort RPO is meaningless when no RAM is restored).
ALTER TABLE sessions ADD COLUMN sla_tier text NOT NULL DEFAULT 'standard'
  CHECK (sla_tier IN ('standard','premium','best_effort'));
ALTER TABLE quotas ADD COLUMN sla_tier text NOT NULL DEFAULT 'standard'
  CHECK (sla_tier IN ('standard','premium','best_effort'));

-- Recovery mode: 'cold_only' = zero disk loss but full RAM loss on recovery;
-- 'warm_allowed' = may roll disk back to the last paired snapshot to preserve RAM.
-- Per-org ceiling in quotas.recovery_mode; per-session can be lower (never higher).
-- Per-session DEFAULT 'warm_allowed' so the design goal of preserving running
-- programs is the default. The org-level ceiling in quotas is the gate.
ALTER TABLE sessions ADD COLUMN recovery_mode text NOT NULL DEFAULT 'warm_allowed'
  CHECK (recovery_mode IN ('cold_only','warm_allowed'));
-- Org ceiling. New orgs start at cold_only until R0 picks an engine and the
-- operator flips this per-org; existing orgs upgrade by setting explicitly.
ALTER TABLE quotas ADD COLUMN recovery_mode text NOT NULL DEFAULT 'cold_only'
  CHECK (recovery_mode IN ('cold_only','warm_allowed'));

-- best_effort is always cold_only at the session level. Enforce it in the
-- schema so the API cannot store an inconsistent row.
ALTER TABLE sessions ADD CONSTRAINT besteffort_is_cold
  CHECK (NOT (sla_tier = 'best_effort' AND recovery_mode = 'warm_allowed'));

-- Tier-1 crash counter (resets on successful warm resume).
ALTER TABLE sessions ADD COLUMN crash_count int NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN last_crash_at timestamptz;

-- Set true when a snapshot was skipped or stayed Pending past rpo_seconds.
-- Cleared at the next LocalDurable snapshot. Paged for premium, ticketed for
-- standard (see §8.3).
ALTER TABLE sessions ADD COLUMN rpo_at_risk boolean NOT NULL DEFAULT false;

CREATE INDEX sessions_recovering_idx ON sessions (state, last_crash_at)
  WHERE state = 'recovering';
CREATE INDEX sessions_hibernated_idx ON sessions (state, snapshot_taken_at)
  WHERE state = 'hibernated';
CREATE INDEX sessions_archived_idx ON sessions (state, snapshot_taken_at)
  WHERE state IN ('cold_archived','archived');
```

### 3.2 `migrations/0028_leases.sql`

```sql
CREATE TABLE leases (
    host_id          uuid PRIMARY KEY,
    owner_instance   text NOT NULL,         -- controld instance id (for split-brain on leases themselves)
    generation       bigint NOT NULL,        -- bumped on every takeover
    acquired_at      timestamptz NOT NULL,
    expires_at       timestamptz NOT NULL,   -- now() + 3s at each heartbeat
    last_renewed_at  timestamptz NOT NULL,
    state            text NOT NULL DEFAULT 'held'
                     CHECK (state IN ('held','suspected','released')),
    suspected_at     timestamptz,
    confirmed_dead_at timestamptz,
    bmc_hostname     text,
    bmc_kind         text CHECK (bmc_kind IN ('ipmi','redfish') OR bmc_kind IS NULL)
);

CREATE INDEX leases_expires_idx ON leases (expires_at)
  WHERE state = 'held';
CREATE INDEX leases_suspected_idx ON leases (suspected_at)
  WHERE state = 'suspected';
```

### 3.3 `migrations/0029_snapshots.sql`

```sql
CREATE TABLE snapshots (
    id                  uuid PRIMARY KEY,
    session_id          uuid NOT NULL REFERENCES sessions(id),
    -- Pair: same instant, atomic.
    disk_snap_id        text NOT NULL,            -- RBD snapshot name
    -- Memory: ref is empty for a base (full snapshot), points at a RADOS/R2
    -- key for a diff. Bytes must be on the same host for Pending (Phase 2),
    -- then uploaded to R2 / RADOS for Durable.
    mem_snap_ref        text NOT NULL,
    mem_snap_size_bytes bigint NOT NULL DEFAULT 0,
    is_full             boolean NOT NULL,         -- true if base, false if diff
    parent_manifest_id  uuid REFERENCES snapshots(id) ON DELETE RESTRICT,  -- diff chain; NULL only for is_full
    -- Manifest integrity.
    manifest_sha256     text NOT NULL,
    ts                  timestamptz NOT NULL DEFAULT now(),
    origin_host_id      uuid NOT NULL,
    cpu_flags           text[] NOT NULL,
    hypervisor          text NOT NULL,
    -- Durability state.
    status              text NOT NULL DEFAULT 'Pending'
                        CHECK (status IN ('Pending','LocalDurable','Durable','Corrupt')),
    hot_copy_at         timestamptz,              -- verified copy in RADOS hot pool
    cold_copied_at      timestamptz,              -- verified copy in R2
    r2_sha256           text,                     -- set when Durable
    -- Verification (background test-restore job).
    last_verified_at    timestamptz,
    last_verify_ok      boolean,
    -- No manifest may be deleted while another depends on it; the FK above
    -- enforces that at commit time.
    CONSTRAINT parent_requires_diff CHECK (
        (parent_manifest_id IS NULL AND is_full) OR
        (parent_manifest_id IS NOT NULL AND NOT is_full)
    )
);

CREATE INDEX snapshots_session_ts_idx ON snapshots (session_id, ts DESC);
CREATE INDEX snapshots_session_durable_idx
    ON snapshots (session_id, ts DESC)
    WHERE status = 'Durable';
CREATE INDEX snapshots_unverified_idx ON snapshots (last_verified_at)
    WHERE last_verified_at IS NULL OR last_verify_ok = false;

-- Retention enforced by puku-snapshot::retention_job. Rule: keep the current
-- base + every diff after it; collapse old diffs into a new base via a
-- background merge (§4.4.4). No manifest is deleted while another depends on
-- it — enforced by parent_manifest_id FK.
```

### 3.4 `migrations/0030_fence_log.sql`

```sql
CREATE TABLE fence_log (
    id           bigserial PRIMARY KEY,
    host_id      uuid NOT NULL,
    session_id   uuid,                       -- the session being protected
    action       text NOT NULL CHECK (action IN ('blocklist','bmc_poweroff','bmc_powercycle','bmc_status')),
    outcome      text NOT NULL CHECK (outcome IN ('ok','failed','timeout')),
    detail       jsonb NOT NULL DEFAULT '{}',
    ts           timestamptz NOT NULL DEFAULT now(),
    requested_by text NOT NULL               -- controld instance id
);

CREATE INDEX fence_log_host_ts_idx ON fence_log (host_id, ts DESC);
CREATE INDEX fence_log_session_ts_idx ON fence_log (session_id, ts DESC)
  WHERE session_id IS NOT NULL;
```

### 3.5 `migrations/0031_packages.sql`

```sql
--Installed package inventory, written by the guest on every install.
-- Enables "rebuild environment" from archived session (Notion §Q2).
CREATE TABLE installed_packages (
    session_id  uuid NOT NULL,
    kind        text NOT NULL CHECK (kind IN ('apt','pip','npm','cargo','go','gem','brew','system')),
    name        text NOT NULL,
    version     text,
    ts          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, kind, name)
);

CREATE INDEX installed_packages_session_idx ON installed_packages (session_id);
```

### 3.6 `migrations/0032_disk_backups.sql`

```sql
CREATE TABLE disk_backups (
    id           uuid PRIMARY KEY,
    session_id   uuid NOT NULL REFERENCES sessions(id),
    kind         text NOT NULL CHECK (kind IN ('full','diff')),
    from_snap    text,                 -- NULL for full
    to_snap      text NOT NULL,        -- RBD snapshot name this export ends at
    r2_key       text NOT NULL,        -- backups/<session>/<ts>.{full|diff}.zst
    size_bytes   bigint NOT NULL,
    sha256       text NOT NULL,
    status       text NOT NULL DEFAULT 'Pending'
                 CHECK (status IN ('Pending','Durable','Corrupt')),
    ts           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT diff_has_from CHECK ((kind = 'full' AND from_snap IS NULL) OR
                                    (kind = 'diff' AND from_snap IS NOT NULL))
);
CREATE INDEX disk_backups_session_ts_idx ON disk_backups (session_id, ts DESC);

ALTER TABLE sessions ADD COLUMN last_disk_backup_at timestamptz;
ALTER TABLE quotas   ADD COLUMN disk_backup_interval_s int NOT NULL DEFAULT 3600;
```

---

## 4. Crate specifications

### 4.1 `crates/puku-volume/`

**Purpose.** Single trait for volume operations, two backends. Replacing direct host-disk access in `session_actor.rs` is the precondition for stateless workers.

**File layout.**

| File | Contents |
|---|---|
| `Cargo.toml` | See §4.1.1 |
| `src/lib.rs` | Re-exports `Volume`, `VolumeBackend`, error type |
| `src/traits.rs` | `trait Volume`, `trait VolumeBackend` |
| `src/types.rs` | `VolumeId`, `HostId`, `SnapId`, `DevicePath`, `FenceToken` |
| `src/local.rs` | `LocalBackend` (existing bind-mount) |
| `src/rbd.rs` | `RbdBackend` (Ceph RBD) |
| `src/fence.rs` | `blocklist(host)` and `unblocklist(host)` shared by both backends |
| `src/error.rs` | `VolumeError` enum |

#### 4.1.1 `Cargo.toml`

```toml
[package]
name = "puku-volume"
version = "0.1.0"
edition = "2021"

[dependencies]
puku-cloud-proto = { path = "../puku-cloud-proto" }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4","serde"] }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "2"
tracing = "0.1"

[target.'cfg(target_os = "linux")'.dependencies]
# RBD via librados/librbd. rusted by ceph-rust or shelling out to rbd-ctl.
# Start with shelling out (simpler) — switch to librbd later.
# No third-party crate needed; std::process::Command to /usr/bin/rbd.
```

#### 4.1.2 Public API (`src/traits.rs`)

```rust
use std::path::Path;
use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VolumeId(pub String); // "rbd-sessions/<uuid>"

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostId(pub Uuid);

#[derive(Debug, Clone)]
pub struct SnapId {
    pub volume: VolumeId,
    pub name: String,            // "<uuid>@<unix_ts>"
}

#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    #[error("volume {0} not found")]
    NotFound(VolumeId),
    #[error("host {0} already fenced")]
    HostFenced(HostId),
    #[error("i/o error: {0}")]
    Io(String),
    #[error("fence failed: {0}")]
    Fence(String),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
}

#[async_trait]
pub trait VolumeBackend: Send + Sync {
    /// Create a new session volume as CoW clone of `base`.
    async fn create(
        &self,
        session_id: Uuid,
        base: &SnapId,
        on_host: HostId,
    ) -> Result<VolumeId, VolumeError>;

    /// Map the volume onto `host`. Idempotent if already mapped.
    async fn attach(
        &self,
        vol: &VolumeId,
        host: HostId,
    ) -> Result<PathBuf, VolumeError>;

    /// Unmap from `host`. Idempotent.
    async fn detach(
        &self,
        vol: &VolumeId,
        host: HostId,
    ) -> Result<(), VolumeError>;

    /// Take a crash-consistent snapshot of the volume.
    /// Returns the SnapId; the disk must be flushed before this call
    /// (guest issues `sync` over vsock first).
    async fn snapshot(
        &self,
        vol: &VolumeId,
        on_host: HostId,
    ) -> Result<SnapId, VolumeError>;

    /// Move a volume's mapping from `from` to `to`.
    /// Backend may implement as detach+attach or live migration.
    async fn relocate(
        &self,
        vol: &VolumeId,
        from: HostId,
        to: HostId,
    ) -> Result<PathBuf, VolumeError>;

    /// Add `host` to the Ceph blocklist. Old host loses RBD I/O.
    /// MUST be called before any failover that writes the same volume.
    async fn fence(&self, host: HostId) -> Result<(), VolumeError>;

    /// Remove `host` from Ceph blocklist (post-recovery).
    async fn unfence(&self, host: HostId) -> Result<(), VolumeError>;

    /// Fast health check — does `host` currently have I/O access to `vol`?
    async fn is_reachable(
        &self,
        vol: &VolumeId,
        host: HostId,
    ) -> Result<bool, VolumeError>;
}

#[async_trait]
pub trait Volume: Send + Sync {
    fn backend(&self) -> &dyn VolumeBackend;
}
```

#### 4.1.3 `RbdBackend` acceptance criteria

| Test | Expected |
|---|---|
| `create → attach → write a string → detach → attach on other host → read string` | String is identical. CoW clone works. |
| `attach on host, then fence(host) → try to write → I/O error returned` | Old host cannot write. |
| `fence(host) → unfence(host) → write succeeds` | Blocklist is reversible. |
| `snapshot → sha256 of base → revert to snapshot → sha256 matches` | Snapshots are crash-consistent. |

#### 4.1.4 Operations cookbook (CLI calls for RbdBackend)

```
rbd clone rbd-base/puku-agent-0.1.0@snap rbd-sessions/<session_id>
rbd map rbd-sessions/<session_id>            # on host
rbd unmap /dev/rbd<N>                          # on host
rbd snap create rbd-sessions/<session_id>@<ts>
rbd blocklist add <client_hostname>            # fence
rbd blocklist remove <client_hostname>         # unfence
```

Implement these via `std::process::Command` with these env vars:
- `CEPH_CONFIG` — path to ceph.conf
- `CEPH_USER` — "puku"
- `RBD_POOL_BASE`, `RBD_POOL_SESSIONS`, `RBD_POOL_COLD`

---

### 4.2 `crates/puku-leases/`

**Purpose.** Detect host death within 1–3 seconds. Required by F4, F5, F6.

**File layout.**

| File | Contents |
|---|---|
| `Cargo.toml` | workspace deps, async-trait |
| `src/lib.rs` | `LeaseService`, `LeaseState` |
| `src/heartbeat.rs` | Worker-side heartbeat task |
| `src/sweeper.rs` | Control-plane sweeper that marks expired leases `suspected` |
| `src/bmc_probe.rs` | Optional BMC reachability check on suspect |

#### 4.2.1 Public API

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Held,        // healthy
    Suspected,   // expired but BMC not confirmed dead
    Released,    // confirmed dead or hand-off complete
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub host_id: HostId,
    pub generation: u64,
    pub state: LeaseState,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub bmc: Option<BmcEndpoint>,
}

#[derive(Debug, Clone)]
pub struct BmcEndpoint {
    pub hostname: String,
    pub kind: BmcKind,    // Ipmi | Redfish
    pub username: String,
    pub password: String, // from secret store
}

pub trait LeaseService: Send + Sync {
    /// Acquire a lease. Fails if another controld instance holds it
    /// (advisory lock per host_id).
    async fn acquire(&self, host_id: HostId, bmc: Option<BmcEndpoint>) -> Result<Lease, LeaseError>;

    /// Renew. Bumps generation. Must be called every 1 s.
    async fn renew(&self, lease: &Lease) -> Result<Lease, LeaseError>;

    /// Release. Call on clean shutdown.
    async fn release(&self, lease: &Lease) -> Result<(), LeaseError>;

    /// Look up current state of `host_id`.
    async fn lookup(&self, host_id: HostId) -> Result<Option<Lease>, LeaseError>;

    /// Mark a lease suspected (control plane only).
    async fn mark_suspected(&self, host_id: HostId) -> Result<(), LeaseError>;
}
```

#### 4.2.2 Worker-side heartbeat contract

```rust
// In puku-workerd main loop:
loop {
    lease_service.renew(&lease).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
}
```

If `renew` fails (controld unreachable, advisory lock contention), the worker **must**:
1. Log `lease_lost` with `host_id`.
2. Set its own VMs to **read-only** (snap a snapshot, then stop writing).
3. Continue trying to renew; if N=3 consecutive fails, **self-shutdown** the VMs (they're in a split-brain state anyway).

#### 4.2.3 Sweeper contract

```rust
// In controld, every 1 s:
for lease in leases WHERE state = 'held' AND expires_at < now() {
    mark_suspected(lease.host_id).await?;
    // Optionally probe BMC to confirm death.
    // If BMC unreachable for > 2 probe attempts → mark confirmed dead.
}
```

#### 4.2.4 Acceptance criteria

| Test | Expected |
|---|---|
| `acquire(HostA) → renew() × 5 → lookup()` | state=Held, expires_at advances |
| `acquire(HostA) → stop renewing → wait 4 s → lookup()` | state=Suspected |
| `acquire(HostA) → stop renewing → wait 4 s → BMC probe fails → lookup()` | state=Released, generation bumped |
| `acquire(HostA) → mark_suspected → renew()` from worker | Fails (lease is no longer held) |

---

### 4.3 `crates/puku-fence/`

**Purpose.** Make split-brain impossible by cutting the old host's write access before any failover.

**File layout.**

| File | Contents |
|---|---|
| `Cargo.toml` | deps + reqwest (Redfish), std::process (ipmitool) |
| `src/lib.rs` | `Fence` trait |
| `src/ceph.rs` | Ceph blocklist via rbd CLI |
| `src/ipmi.rs` | IPMI power-cycle via ipmitool |
| `src/redfish.rs` | Redfish power-cycle via HTTPS |
| `src/audit.rs` | Writes to `fence_log` table |

#### 4.3.1 Public API

```rust
#[async_trait]
pub trait Fence: Send + Sync {
    /// Block the host from writing ANY RBD volume.
    /// Returns Ok only after blocklist is confirmed.
    async fn blocklist(&self, host_id: HostId) -> Result<(), FenceError>;

    /// Block + optional BMC power cycle.
    /// Returns Ok only after both succeed (or BMC is not configured).
    async fn fence(
        &self,
        host_id: HostId,
        bmc: Option<&BmcEndpoint>,
    ) -> Result<FenceReceipt, FenceError>;

    /// Reverse blocklist (post-recovery).
    async fn unfence(&self, host_id: HostId) -> Result<(), FenceError>;
}

#[derive(Debug, Clone)]
pub struct FenceReceipt {
    pub host_id: HostId,
    pub blocklisted_at: chrono::DateTime<chrono::Utc>,
    pub bmc_action: Option<String>,  // "power_off"|"power_cycle"|"none"
    pub audit_log_id: i64,
}
```

#### 4.3.2 Operational rule (HARD)

**No `recovery.rs` code may call `attach` or `snapshot` for a session until `Fence::fence` has returned `Ok` for the suspected host.**

This rule is enforced by:
- `Fence::fence` returns a `FenceReceipt`.
- `recovery.rs` passes the receipt to `volume.attach(...)`.
- Both logged to `audit_log`.

#### 4.3.3 Acceptance criteria

| Test | Expected |
|---|---|
| `fence(HostA) → write to volume attached on HostA` | I/O error or blocklist message |
| `fence(HostA) → unfence(HostA) → write to volume on HostA` | Succeeds |
| `fence(HostA) with BMC unreachable` | Returns `FenceError::BmcUnreachable` after blocklist still succeeds (don't fail entire fence for BMC alone — blocklist is the primary fence) |
| `fence(HostA) → audit log entry` | One row in `fence_log` with outcome=ok, action=blocklist |

---

### 4.4 `crates/puku-snapshot/`

**Purpose.** Take crash-consistent disk+memory snapshots together, verify them, retain them safely, test-restore nightly. The design is one unit: trigger, capture, durability, retention, restore. They are tightly coupled — a mistake in one breaks the others.

**File layout.**

| File | Contents |
|---|---|
| `Cargo.toml` | deps + sha2, zstd, sqlx |
| `src/lib.rs` | Re-exports |
| `src/capture.rs` | `capture_paired()` — atomic disk+mem, in-memory buffer for mem so phase 3 has stable bytes |
| `src/restore.rs` | `restore()` — pick durable manifest, verify chain, apply |
| `src/manifest.rs` | `Manifest` struct + sha256 + `durable` field |
| `src/compaction.rs` | Overlay-merge compactor (§4.4.4): trigger (depth≥8 or diff-sum≥RAM×0.5), read+overlay, register B', cut-over with re-parenting in one transaction. Off the VM critical path. |
| `src/retention.rs` | Newest-first reaper that deletes malformed-or-aged manifests after compaction; never deletes out of order (children first). Old "chain collapse" wording replaced — the merge is in `compaction.rs`. |
| `src/test_restore.rs` | Nightly background restore verifier |
| `src/idle_signal.rs` | vsock listener for "between tool calls" |
| `src/triggers.rs` | The **only** place that decides when to snapshot. `effective_recovery_mode()` + `rpo_seconds(tier)` live here; every other call site reads the same constant. Includes the idle optimization. |
| `src/disk_backup.rs` | Periodic off-cluster backup (`rbd export`/`export-diff` to R2, 24-diff compaction, restore by `rbd import` + `import-diff`). See §4.7. |
| `src/chain.rs` | Diff-chain bookkeeping: which manifest is the current base, which to compact next |

#### 4.4.1 Public API

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotStatus {
    /// Manifest durable in Postgres; memory bytes exist ONLY in the workerd
    /// RAM buffer. Visibility-only. NEVER restorable.
    Pending,
    /// Compressed memory file fsynced on the origin host's NVMe, sha256 recorded.
    /// Restorable on the origin host only (local tier, F1/F2).
    LocalDurable,
    /// A verified copy exists off the origin host (RADOS hot pool, or R2 when
    /// the hot pool is off). Restorable anywhere.
    Durable,
    /// Sha256 mismatch on background verify. Never restorable.
    Corrupt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: Uuid,
    pub session_id: Uuid,
    pub disk_snap_id: String,         // "rbd-sessions/<id>@<ts>"
    pub mem_snap_ref: String,         // R2 / RADOS key
    pub mem_snap_size_bytes: u64,     // 0 until Durable
    pub is_full: bool,                // true = base (no parent); false = diff
    pub parent_manifest_id: Option<Uuid>,  // diff chain link
    pub ts: chrono::DateTime<chrono::Utc>,
    pub origin_host_id: Uuid,
    pub cpu_flags: Vec<String>,
    pub hypervisor: String,
    pub status: SnapshotStatus,
    pub sha256: String,               // of all of the above
    pub r2_sha256: Option<String>,    // set when Durable
}

#[async_trait]
pub trait SnapshotService: Send + Sync {
    /// Capture a paired snapshot. Returns a `Pending` manifest. Caller
    /// must call `mark_durable()` after the async R2 upload completes.
    async fn capture_paired(
        &self,
        session_id: Uuid,
        vol: &VolumeId,
        on_host: HostId,
        origin_cpu_flags: &[String],
        hypervisor: &str,
    ) -> Result<Manifest, SnapError>;

    /// Phase 3, step 1: local file fsynced and hashed. Pending -> LocalDurable.
    async fn mark_local_durable(&self, manifest_id: Uuid) -> Result<Manifest, SnapError>;

    /// Phase 3, step 2: off-host copy verified. LocalDurable -> Durable.
    async fn mark_durable(&self, manifest_id: Uuid) -> Result<Manifest, SnapError>;

    /// Restore from the latest Durable manifest. Falls back to previous
    /// durable in chain, then cold resume (RBD + events.ndjson).
    async fn restore(
        &self,
        session_id: Uuid,
        to_host: HostId,
    ) -> Result<RestoreOutcome, SnapError>;

    /// Test-restore the latest Durable manifest in scratch space.
    async fn verify_latest(&self, session_id: Uuid) -> Result<bool, SnapError>;
}

#[derive(Debug)]
pub enum RestoreOutcome {
    /// Disk + memory restored from a Durable manifest (or `LocalDurable` on
    /// the origin host — see §4.4.2). RPO = age of that manifest.
    Warm(Manifest),
    /// Memory missing or engine-incompatible (diff unsupported, cpu mismatch).
    /// Disk is the **current RBD head** — no rollback, no package loss.
    /// Agent re-reads transcript for context; in-flight tools re-run.
    ColdHead(VolumeId),
    /// Disk volume lost. Restored disk from R2 disk archive (sha256 verified).
    /// Memory not available; full RAM loss.
    FromArchive(Manifest),
    /// Disk lost AND archive lost. Re-created disk from base image, replayed
    /// `installed_packages`, replayed event log from `last_seq`. This is the
    /// worst-case floor — it loses data files but keeps the environment and
    /// the conversation.
    Rebuilt(VolumeId),
    /// All options exhausted. Caller decides: try another host, fail session, etc.
    Failed(String),
}
```

#### 4.4.2 Capture sequence (atomic from the guest's perspective)

Three phases. The invariant: the bytes that land in `/var/lib/puku/snap-cache/.../mem` and ultimately in R2 are the bytes captured at the freeze instant — not a post-resume dump. The mechanism: copy memory into a stable host-side buffer during Phase 1, write that buffer to local NVMe in Phase 2, and upload the same buffer to R2 in Phase 3. The VM only ever sees a freeze, a thaw, and a (possibly delayed) pause.

```
PHASE 1 — FREEZE (VM paused, all of this is critical-path, p95 ≤ 250 ms)
  t=0      Trigger fires (idle, or force-tick — see §4.4.3).
  t=0+1ms  Workerd sends `sync(1)` over vsock; guest fsyncs.
  t=0+5ms  Workerd calls `vm.pause()` via the hypervisor API.
  t=0+5ms  Workerd captures RBD snapshot (`rbd snap create`).       [disk atomic point]
  t=0+15ms Workerd captures memory into a HOST-SIDE BUFFER:
           - diff (warm path): read dirty pages from the VMM's page-tracking
             bitmap into a 64-byte-aligned host buffer; keep the buffer alive
             past resume.
           - first-time base only: read ALL memory into a host buffer (see
             "Where full snapshots live" below — this is the one exception).
  t=0+90ms Workerd computes manifest SHA-256 from the buffer + disk snap id.

PHASE 2 — PERSIST (VM paused, manifest must be durable before resume)
  t=0+95ms  Workerd writes manifest to local NVMe: snap-cache/<session>/<ts>.json
  t=0+100ms Workerd fsyncs the manifest file.
  t=0+105ms Workerd writes manifest to Postgres with status=Pending.
  t=0+110ms Workerd calls `vm.resume()`.        <-- pause ends here (p50 ≤ 120 ms)
  t=0+111ms Workerd acks `idle_for_snapshot` over vsock.

PHASE 3 — DURABLE (VM running, does not block the guest)
  t=0+0.2s  Compress buffer (zstd); write snap-cache/<session>/<ts>.mem.zst on local NVMe; fsync.
            UPDATE status = 'LocalDurable'.                       (local restore is now allowed)
  t=0+0.5s  Write the same bytes to the RADOS hot pool (puku-snap-hot, 3 replicas),
            object key snapshots/<session>/<ts>.mem.zst. Read back and verify sha256.
            UPDATE status = 'Durable', hot_copy_at = now().       (remote restore is now allowed)
  t=0+2s…   Async cold copy: multipart upload to R2 + manifest JSON.
            UPDATE cold_copied_at = now().                        (does NOT gate Durable)
```

**Durable rule.** `Durable` means **a verified copy exists off the origin host**:
- Hot pool enabled (default for `premium`, recommended for `standard`): Durable = the RADOS copy is verified. Typical lag is 1–2 s.
- Hot pool not enabled: Durable = the R2 copy is verified. Typical lag is about 30 s.

The R2 copy is the cold archive. It never gates Durable when the hot pool is on.

**Where full snapshots live.** The first snapshot for a session is a full mem dump — but it is taken either from a **pre-built boot-time template** (so the diff against it is tiny from the start) or at the **first idle point after boot** (so the VM is quiesced when we read all 8 GB). After that, every snapshot is a diff against the current base. There is no "chain collapse" inside Phase 1. Compaction is a separate background merge (§4.4.4).

**Pause budget: ≤ 120 ms p50, ≤ 250 ms p95.** Anything over 250 ms is an alert. Full-snapshot pauses (rare; first only) are exempt — they run at first idle, not on a hot tool call.

**Backlog cap (do not pile up buffers).** At a 5 s RPO with ~30 s durability lag, up to 6 buffers per session sit in `/var/lib/puku/snap-cache/` waiting for upload. The capture path enforces a per-session cap (`SNAPSHOT_BUFFER_MAX = 16`); if the cap is reached, the trigger skips the snapshot and logs `snapshot_backlog_skip_total`. Backlog is published as `puku_snapshot_pending_buffers{session_id}`.

**A skipped snapshot is an RPO violation in progress, not a log line.** Skipping is the right thing to do for the buffer budget, but it is exactly the kind of silent degradation a reliability spec exists to prevent. Therefore:

- Each skip increments `puku_snapshot_backlog_skip_total{session_id, tier}`.
- If a premium session skips while `rpo_at_risk = false`, the sweeper immediately sets `rpo_at_risk = true` (the same column the cron job would set after `rpo_seconds`); premium is **paged** within 60 s (§8.3, `RPOAtRisk`).
- If a standard session skips while `rpo_at_risk = false`, the same column is set; standard gets a **ticket**, not a page.
- The skip is also logged with `session_id`, `host_id`, `pending_buffers`, and `last_durable_at`, so the runbook can correlate it with the upload lag that caused it.

**Why the buffer lives across phases.** Phase 3 compresses and uploads the same buffer that Phase 1 captured and Phase 2 fsynced — the bytes in `/var/lib/puku/snap-cache/.../mem` are the frozen instant, not a post-resume dump. The manifest refers to `mem_snap_ref` (the R2 / RADOS key) but is `Pending` until the local file is fsynced (`LocalDurable`), then `Durable` once the off-host copy is verified and `mark_durable()` runs.

#### 4.4.2a Diff memory snapshots (load-bearing, but support depends on the engine)

**Requirement.** Memory snapshots form an **incremental chain**: each diff is relative to the **previous snapshot**, not to a fixed base. Restore walks the chain from the latest back to the most recent full base. The pause per snapshot is O(dirty pages) — the same regardless of chain depth. Restore is O(base + sum of diff sizes); depth is bounded by the compaction rule below.

**Why incremental, not "diff-only against base".** A diff-against-base rule needs the full base to be intact for every diff to be useful; a 100-deep chain would still read most of the disk. Firecracker's `track_dirty_pages` produces diffs against the previous snapshot, and that is what we use. Compaction (§4.4.4) merges the chain off the critical path.

**Pause budget.** The pause numbers below are targets to benchmark in Phase R0 (§11). Real numbers depend on dirty-page rate, VM size, NVMe vs SSD, and hypervisor version. The benchmark, not the table, is the spec.

| Approach | Pause target (8 GB VM, idle) | Noticeable? |
|---|---|---|
| Full memory snapshot, uncompressed | ~3 s | Yes — only at first idle after boot |
| Full memory snapshot, zstd | ~1 s | Yes — only at first idle after boot |
| Diff against previous | ~50 ms target | No |
| No memory snapshot (cold resume only) | 0 ms | No — but loses RAM |

**Per-hypervisor support — verified status as of this writing:**

| Hypervisor | Diff memory | Evidence | Verdict |
|---|---|---|---|
| **Cloud Hypervisor (upstream v53)** | **Unverified upstream.** The CH `vm-migration` crate defines a `SnapshotType` enum that *may* include `Full`/`Diff`/`SoftDirty`, but the v53 release notes (release tag `v53.0`, 2026-10) only mention the offloaded snapshot/restore daemon and postcopy live migration — **no upstream mention of diff memory snapshots as a documented feature**. Some blog posts describing these exist for a Tencent fork (CubeHypervisor), not upstream CH. | Phoronix CH v53 coverage; CH `v53.0` release notes; CSDN/Gitcode blogs about CH snapshots | **Verify against exact CH version in CI before shipping warm resume on CH.** |
| **libkrun / msb (current repo engine)** | **Developer preview.** Diff snapshots in msb's SDK are still flagged developer preview. | msb release notes; `sdk/docs/snapshot.md` | **Verify against exact SDK version in CI.** |
| **Firecracker** | **Verified upstream.** `track_dirty_pages` + `enable_diff_snapshots` flags documented in upstream `firecracker/docs/snapshotting/snapshot-support.md`. The diff is **relative to the previous snapshot**, not to a fixed base; in production use by e2b (see `e2b-dev/firecracker` fork + rebase tool). | Upstream Firecracker docs; HN discussion; e2b source | **Safe to ship warm resume on Firecracker.** |
| **QEMU/KVM** | Native dirty-page tracking; `migrate` supports postcopy. Diffs also relative to the previous snapshot. | QEMU docs | **Safe but heavier.** |

**Implication for the engine decision.** Upstream CH is unverified for diff memory snapshots; Firecracker is verified. The engine decision is deferred to **Phase R0 — Hypervisor Bake-Off** (§11), which benchmarks all three on real hardware with the actual VM sizes we'll ship. Until R0 produces numbers, every org's `quotas.recovery_mode` is forced to `cold_only`; warm is opt-in per org once R0 closes.

**Acceptance criteria — these are now R0 outputs, not pre-decided:**

| Test | Expected (subject to R0) |
|---|---|
| `bench_capture(engine, vm_size_mib=8192, mode=Diff)` | Pause p50, p95 measured; stored in `bench/` directory |
| `bench_restore(engine, chain_depth=K, vm_size_mib=8192)` | Restore time p50, p95 measured |
| `capture_paired()` rejected on engine with no diff support | `Err(SnapError::DiffUnsupportedOnEngine)` |
| `restore()` on a Pending or LocalDurable manifest from a remote host | Rejected with `Err(SnapError::ManifestNotDurable)` |

#### 4.4.3 Snapshot triggers — single source of truth for the RPO interval

**One constant, derived from tier.** RPO lives in code as a constant keyed on `sla_tier`. There is no `rpo_target_s` column to drift out of sync with the promise.

```rust
// crates/puku-snapshot/src/triggers.rs
fn rpo_seconds(tier: &SlaTier) -> Option<u32> {
    match tier {
        SlaTier::Premium     => Some(5),
        SlaTier::Standard    => Some(60),
        // best_effort sessions are cold_only by default; the warm RPO is
        // meaningless when no RAM is restored.
        SlaTier::BestEffort  => None,
    }
}
```

**RPO promise per tier (matches §0.1):**

| Tier | Warm RPO (max loss) | `recovery_mode` | What RPO means for `cold_only` |
|---|---|---|---|
| `premium` | 5 s | `warm_allowed` | same — only the snapshot-affected RAM is lost |
| `standard` | 60 s | `warm_allowed` (default) | same |
| `best_effort` | 300 s | `cold_only` (forced) | disk head intact; full RAM loss on any crash |

**Trigger rules — one source:**

```rust
/// One function decides the effective mode. Nothing else may decide it.
pub fn effective_recovery_mode(
    sla: SlaTier,
    session_pref: RecoveryMode,   // sessions.recovery_mode
    org_ceiling: RecoveryMode,    // quotas.recovery_mode
    worker_warm_ok: bool,         // engine advertises warm + R0-validated
) -> RecoveryMode {
    use RecoveryMode::*;
    if sla == SlaTier::BestEffort || !worker_warm_ok { return ColdOnly; }
    // never above the org ceiling; session can only lower it
    if session_pref == ColdOnly || org_ceiling == ColdOnly { ColdOnly } else { WarmAllowed }
}

fn rpo_seconds(tier: &SlaTier) -> Option<u32> {
    match tier {
        SlaTier::Premium    => Some(5),
        SlaTier::Standard   => Some(60),
        SlaTier::BestEffort => None,   // always cold_only
    }
}

async fn try_snapshot_if_due(&self, s: &Session, q: &Quota, w: &Worker) {
    // Cold sessions are never paused for memory snapshots.
    if effective_recovery_mode(s.sla_tier, s.recovery_mode, q.recovery_mode, w.warm_resume_supported)
        != RecoveryMode::WarmAllowed { return; }
    let Some(interval) = rpo_seconds(&s.sla_tier) else { return };
    let age = (Utc::now() - s.last_snapshot_taken_at).num_seconds();
    if age < interval as i64 { return; }
    self.capture_paired(/* … */).await;
}
```

**The idle optimization (optional, not a contract):** If the agent has been idle (no in-flight tool call, no in-flight LLM) for ≥ 2 s AND age > `rpo_seconds / 4`, take the snapshot early. This reduces mid-tool-call pauses. The RPO is still bounded by the constant — the idle trigger is a UX improvement, not a guarantee.

**The "5 s premium" feasibility check.** With `rpo_seconds = 5`, the trigger fires every 5 s. Each trigger pauses the VM (≤ 250 ms p95). That is 5 % of wall-clock time spent paused. For a 100 ms p50 pause, it is 2 %. **This is the cost of the 5 s RPO promise; it is not free.** Document it in the org-facing SLA: "5 s RPO implies up to ~5 % VM pause time for snapshotting on premium tier."

**Honest RPO promise.** Until `status = Durable`, the manifest exists but the memory bytes are NOT in remote storage. For **local** restore on the same host, a `LocalDurable` manifest IS usable: the file is on the local NVMe and we re-hash it before applying. For **remote** restore, only `Durable` is accepted. The status table and RPO table below restate this.

| Status | Local restore (origin host) | Remote restore (any host) |
|---|---|---|
| `Pending` | No | No |
| `LocalDurable` | Yes (sha256 re-verified before apply) | No |
| `Durable` | Yes | Yes |
| `Corrupt` | No | No |

| Failure | RPO actually delivered |
|---|---|
| Sandbox crash (local-tier, F1/F2) | `rpo_seconds` + ~0.2 s (until `LocalDurable`) |
| Host loss (remote-tier, F4/F5), hot pool on | `rpo_seconds + ~1–2 s` |
| Host loss (remote-tier, F4/F5), R2 only | `rpo_seconds + ~30 s` |

| Premium host-loss lag | ~1–2 s (RADOS hot pool required; see §4.4.3a) — metric `puku_durability_lag_seconds{quantile}` |
| Standard host-loss lag | ~1–2 s with the hot pool, ~30 s without it |

#### 4.4.3a Hot snapshot tier (RADOS pool)

**Purpose.** Make `Durable` fast (about 1 s) without waiting for R2.

| Item | Value |
|---|---|
| Pool | `puku-snap-hot`, replicated, `size 3`, `min_size 2`, application `rados` |
| Contents | Current base + the diffs after it, for every warm session. Compaction (§4.4.4) bounds it. |
| Access | `client.puku-cap` has `allow rw pool=puku-snap-hot` |
| Capacity | `Σ over warm sessions (compressed base + Σ diffs since base) × 3`. Alert at 70 % full (§8). |
| Eviction | An object is deleted from the pool only after (a) it is no longer referenced by a live chain and (b) `cold_copied_at` is set. |
| If the pool is full or down | New captures still complete as `LocalDurable`. Remote restore falls back to the latest manifest with a verified R2 copy. `premium` SLA is **not** honoured while this holds; alert `HotPoolUnavailable`. |

**Premium gate.** `POST /v1/sessions` for `sla_tier = premium` returns `409 premium_requires_hot_pool` when the org's hot pool is disabled or the pool health check fails (see P4 for the other gates).

#### 4.4.4 Retention — incremental chain + background merge

**The chain.** Each snapshot's `parent_manifest_id` points at the previous snapshot (full or diff). Restore walks from the latest back to the most recent `is_full = true`. Depth is bounded by the compaction rule below, so restore time stays predictable.

**No manifest may be deleted while another depends on it.** `parent_manifest_id` is declared `REFERENCES snapshots(id) ON DELETE RESTRICT`. Because every diff points at its predecessor, old chains must be deleted **newest first** (children before parents). The retention job never deletes out of order.

**Diffs are sparse page maps.** A diff holds `(guest_physical_offset, page_bytes)` records for pages dirtied since the previous snapshot. A merge is therefore a **page overlay by offset, newest wins**. It is not byte concatenation.

**Compaction rule (background merge).**

1. **Trigger.** When chain depth reaches `MERGE_DEPTH = 8`, or the sum of diff sizes since the last base exceeds `MERGE_THRESHOLD_BYTES = RAM × 0.5`, schedule a compaction job for the session.
2. **Pick the merge point.** `N` = the newest manifest that is `Durable` at schedule time. Captures keep happening during the merge; they are not part of it.
3. **Merge.** Read the current full base, then overlay every diff from the base up to and including `N`, in order. Write the result as a new full image `B'` under a new key (RADOS hot pool first, R2 cold copy async).
4. **Register.** Insert a manifest row for `B'`:
   `is_full = true`, `parent_manifest_id = NULL`, `ts = N.ts`, `disk_snap_id = N.disk_snap_id`, `status = Pending`, `sha256` over the merged bytes. It becomes `LocalDurable`/`Durable` through the normal path (§4.4.2).
5. **Cut over (one transaction, only after `B'` is `Durable`).**
   ```sql
   BEGIN;
   -- snapshots taken while the merge ran point at N; re-parent them onto B'
   UPDATE snapshots SET parent_manifest_id = :b_new
    WHERE parent_manifest_id = :n;
   -- delete the old chain from N down to the old base, newest first
   DELETE FROM snapshots WHERE id = :n;
   DELETE FROM snapshots WHERE id = :n_minus_1;
   -- ... one statement per manifest, ending with the old base ...
   COMMIT;
   ```
   After commit, reap the cold/hot objects and the RBD snapshots (`disk_snap_id`) of the deleted manifests. **Never** delete the RBD head.
6. **Failure.** If the job fails, or `B'` ends `Corrupt`, nothing is deleted. The old chain stays valid and `snapshot_compaction_failed_total` increments (alert in §8).

**Invariant.** At no instant is any `Durable` manifest unrestorable, and capture is never blocked by compaction.

**Compaction is off the VM's critical path.** It runs in a worker that may be any host. The VM is not paused.

**Per-session retention override.** Sessions in `archived` state keep ALL durables in R2 / RADOS (not just the post-compaction chain) so that the disk-archive R2 object plus every memory snapshot allow a full restore. The chain in Postgres may still be capped after compaction; the cold-storage bytes for older durables stay until the disk archive is reaped.

**Acceptance criteria:**

| Test | Expected |
|---|---|
| Create 9 snapshots in a row | Compaction fires once at depth 8. After `B'` is Durable, the old base and diffs 1..N are reaped newest-first; chain is `[B' → diff N+1 …]`. |
| Overlay merge correctness | Random page writes across 12 snapshots; restoring `B'` is **byte-identical** to restoring by applying the diffs sequentially. |
| Capture during merge | Take a snapshot while the merge runs. After cut-over it is re-parented onto `B'` and restores correctly. |
| Delete a manifest that is some other manifest's parent | FK rejects it; retention logs `snapshot_retention_blocked` and retries after compaction. |
| Merge failure injected | Old chain untouched, alert fires, session can still warm-restore. |
| Property test: 100 captures | `chain depth ≤ MERGE_DEPTH + SNAPSHOT_BUFFER_MAX` at every point. |
| `restore()` on a Pending manifest (any host) | Rejected with `Err(SnapError::ManifestNotDurable)` |
| Kill workerd between Phase 2 and Phase 3, then restore | Manifest is `Pending` with no bytes. Restore refuses it and falls back to the previous `LocalDurable`/`Durable` manifest. |
| `restore()` on `LocalDurable` on the origin host | Succeeds after sha256 re-verify. |

#### 4.4.5 Test-restore job

```
Nightly at 02:00 local time:
  for session in sessions
   WHERE state IN ('stopped','hibernated','cold_archived','archived')
   ORDER BY random() LIMIT 10:
    let manifest = latest_durable(session.id);
    verify_chain(session.id, manifest)  // walks parent chain, checks each sha256
    metrics.puku_snapshot_test_total.inc(result)
```

`verify_chain` walks the parent chain from the latest back to the base, downloads each from R2, checks sha256, and reports any corrupt link. This is what catches silent bit rot.

#### 4.4.6 Acceptance criteria

| Test | Expected |
|---|---|
| `capture_paired()` on idle VM | manifest written as Pending, memory in host buffer, pause p50 ≤ 120 ms |
| After 30 s async: manifest becomes Durable | `status = Durable`, `r2_sha256` set |
| `restore()` on a Pending manifest | Rejected with `Err(SnapError::ManifestNotDurable)` |
| `restore()` after F1 (VM killed) | Warm restore from latest Durable; session resumes |
| `restore()` with latest Durable corrupt | Walk to previous Durable in chain; restore from there |
| `restore()` with all durables corrupt | Cold restore from RBD base + replay events.ndjson |
| `verify_latest()` on a good manifest | ok=true written |
| `verify_latest()` on a bit-flipped manifest | ok=false written, alert raised |
| Chain collapse test: create 5 snapshots in a row | Manifests 1, 2 (old base + old diff) reaped; manifest 3 promoted to new base; chain length = 3 (new base, diff 4, diff 5) |
| Chain never exceeds 3 durables | Property test: take 100 snapshots, verify `SELECT count(*) FROM snapshots WHERE status='Durable' AND session_id=X` ≤ 3 at every point |

### 4.5 `crates/puku-guestd/src/package_watcher.rs` (in-tree, not separate crate)

**Purpose.** Make `installed_packages` actually populated. Without this, the table is empty and we cannot rebuild an environment from scratch (which is the only way to recover if disk is also lost — the absolute worst case).

**Approach.** A small in-guest agent that watches the package-manager *state*, not the binaries. Watching `/usr/bin/apt-get` only proves the command ran — it does not prove install success or capture the version. The reliable signals are the state directories themselves:

| Source | What we read | Coverage | Notes |
|---|---|---|---|
| `inotify` on `/var/lib/dpkg/status` and `/var/lib/dpkg/info/*.list` | apt install/remove by re-reading `dpkg --get-selections` | apt installs | Authoritative; ignores failures, captures version, no parser needed. |
| `inotify` on `/usr/lib/python3*/site-packages/<pkg>-<ver>.dist-info/`, `~/.local/lib/python*/site-packages/...` | pip packages from their dist-info dirs | pip installs in venvs and user site | Parses `METADATA` for `Version:`. |
| `inotify` on `~/.npm/_cacache/content-v2/`, plus a periodic `npm ls --json --depth=0` | npm packages | npm global + project installs | The cache is the source of truth; `npm ls` covers anything missed. |
| Periodic `cargo install --list`, `go list ...`, `gem list`, `brew list --formula` | cargo, go, gem, brew | Best-effort for less-common managers | Each runs once at cold-start and every 10 min thereafter. |

The watcher emits **changes** (delta), not full state. Inotify on the state directories fires on every real install, so the row table stays current in real time. The cold-start reconcile seeds rows for state that existed before the watcher was attached (e.g. after a warm resume).

**Wire format (vsock):**

```rust
#[derive(Serialize, Deserialize)]
pub enum PackageEvent {
    Installed { kind: PackageKind, name: String, version: Option<String> },
    Removed   { kind: PackageKind, name: String },
}
```

**On the host side** (workerd), a small handler:

```rust
// crates/puku-workerd/src/package_event.rs
async fn handle_package_event(session_id: Uuid, ev: PackageEvent) {
    controld::db::insert_installed_package(session_id, ev).await?;
}
```

**Acceptance criteria**

| Test | Expected |
|---|---|
| `apt-get install -y jq` | Row in `installed_packages (session_id, kind='apt', name='jq', version=…)` within 5 s |
| `pip install requests==2.31.0` | Row with kind='pip', name='requests', version='2.31.0' |
| `pip uninstall requests` | Row deleted |
| `npm install lodash` | Row with kind='npm', name='lodash' |
| Cold start reconcile | All packages present at boot are inserted on first watcher start for the session |

### 4.6 Environment rebuild tool (new: `crates/puku-rebuild/`)

**Purpose.** If a session's disk is truly lost (e.g. archived for years and the R2 archive is gone due to retention), we can still recover the *environment* by replaying `installed_packages` against a fresh base image.

**CLI:** `puku-rebuild <session_id>` — emits a shell script:

```bash
#!/bin/sh
# Auto-generated by puku-rebuild from installed_packages for session <uuid>
apt-get update
apt-get install -y --no-install-recommends git curl wget ca-certificates
pip install --no-cache-dir \
    requests==2.31.0 \
    numpy==1.26.0 \
    pandas==2.1.0
npm install -g lodash@4.17.21
# ... etc, ordered by install time
```

**Does not** recover data files (those are in the transcript + R2 archive). Recovers **the ability to run the same code** the user had running.

### 4.7 Disk backup (`crates/puku-snapshot/src/disk_backup.rs`)

**Purpose.** Ceph (3 replicas) protects against disk and host loss. It does not protect against losing the pool itself (bad upgrade, operator error, cluster loss). This job keeps an independent copy in R2.

**Procedure (per session, every `disk_backup_interval_s`, and always on hibernate).**
1. Take an RBD snapshot `bk-<ts>` of the volume (no VM pause; the guest has been `sync`ed best-effort).
2. If the backup chain is empty: `rbd export` the snapshot, zstd, upload, sha256 → `kind = full`.
   Otherwise: `rbd export-diff --from-snap <last_to_snap> <to_snap>`, zstd, upload, sha256 → `kind = diff`.
3. **Skip** if the diff is empty (no writes since the last backup).
4. Mark `Durable` after the R2 read-back sha256 matches; update `sessions.last_disk_backup_at`.
5. Keep the previous `to_snap` as the next `from_snap`; delete older RBD backup snapshots.

**Compaction.** When a chain reaches 24 diffs, or on the first backup after the session is archived, take a new `full` and then retire the old chain (children first, same rule as §4.4.4).

**Restore (ladder step 3).** `rbd import` the latest full, then `rbd import-diff` each later diff in order, into a fresh image in `rbd-sessions`. Verify every sha256 before applying. Replay the event log from `last_seq` so the conversation is intact.

**Cost note.** Incremental exports are small for AI sessions (mostly package installs and source files). Idle sessions produce empty diffs and are skipped.

**Acceptance criteria**

| Test | Expected |
|---|---|
| Backup of a session with writes | `disk_backups` row `Durable`, `last_disk_backup_at` set |
| Idle session | No row written (empty diff skipped) |
| Chain of 24 diffs | New full taken; old chain retired children-first |
| `T17` `ceph_pool_loss_restore` (§6) | Image deleted; `/resume` restores from backup; files older than the last backup and `installed_packages` are present |

---

## 5. Modified existing code (precise files and lines)

### 5.1 `crates/puku-controld/src/recovery.rs` (new module)

**Path:** `crates/puku-controld/src/recovery.rs`

**Responsibility.** Decide tier-1 vs tier-2 recovery, run fencing, drive snapshots.

#### 5.1.1 Main entry point

```rust
// Tier here is the recovery tier choice (local vs remote), NOT the SLA tier.
// The SLA tier is on session.sla_tier ∈ {standard, premium, best_effort}.
// These are unrelated — keep the names distinct to avoid confusion.

pub enum RecoveryChoice {
    Local,    // restart on the same host (RBD already mapped)
    Remote,   // fence + restart on a different host
}

pub async fn recover_session(
    session_id: Uuid,
    reason: CrashReason,
    lease: LeaseService,
    fence: Fence,
    volume: Arc<dyn VolumeBackend>,
    snapshot: Arc<dyn SnapshotService>,
    db: &PgPool,
) -> Result<RecoveryOutcome, RecoveryError> {
    let session = db.fetch_session(session_id).await?;

    // Step 1: decide recovery choice (NOT the SLA tier).
    let choice = decide_recovery(&session, &lease).await?;

    // Step 2: fence BEFORE any attach on a different host.
    if choice == RecoveryChoice::Remote {
        let fence_receipt = fence.fence(
            session.worker_id.ok_or(...)?.into(),
            lease.bmc_for(session.worker_id).await?,
        ).await?;
        db.record_fence(&fence_receipt).await?;
    }

    // Step 3: restore.
    match choice {
        RecoveryChoice::Local  => restore_local(&session, volume, snapshot).await,
        RecoveryChoice::Remote => restore_remote(&session, volume, snapshot).await,
    }
}

enum Tier { Local, Remote }   // DEPRECATED: kept for source compatibility; use RecoveryChoice in new code
```

#### 5.1.2 Tier decision matrix

| Condition | Tier |
|---|---|
| Worker healthy + same host has memory snapshot cached | Local |
| Worker healthy + no local cache | Local ( snapshot is on R2 anyway) |
| Worker suspect or dead | Remote (after fence) |
| Same VM crashed 3+ times in last 10 min | Remote (eviction) |

### 5.2 `crates/puku-controld/src/leases.rs` (new module)

**Path:** `crates/puku-controld/src/leases.rs`

**Responsibility.** Wrap `puku-leases::LeaseService` with controld-specific config (1 s tick, BMCC probe policy).

| Function | Purpose |
|---|---|
| `start_sweeper()` | Spawns the 1 s tick task |
| `handle_lease_lost(host_id)` | Called when sweeper marks `suspected`; triggers fence |
| `is_host_healthy(host_id)` | Public API for scheduler |

### 5.3 `crates/puku-controld/src/fence.rs` (new module)

**Path:** `crates/puku-controld/src/fence.rs`

**Responsibility.** Wrap `puku-fence::Fence` with controld logging + audit.

### 5.4 `crates/puku-controld/src/sweeper.rs` (new module)

**Path:** `crates/puku-controld/src/sweeper.rs`

**Responsibility.** Background jobs:

| Job | Period | Action |
|---|---|---|
| Lease sweep | 1 s | Mark expired leases `suspected` |
| Tier transitions | 60 s | running→hibernated, hibernated→cold_archived, etc. |
| Snapshot retention | 60 s | Drop manifests older than 3rd-newest, keep in R2 |
| Test-restore | nightly (02:00) | Pick 10 random stopped sessions, verify latest snapshot |
| Reaper | 60 s | Drop reaped volumes from Ceph |

### 5.5 `crates/puku-controld/src/scheduler.rs` (extend)

**Add fingerprint-aware placement.**

```rust
// New function.
async fn pick_worker_with_fingerprint(
    &self,
    session: &Session,
    required_cpu_flags: &[String],
    required_hypervisor: &str,
) -> Option<WorkerId> {
    // Excludes:
    //   - workers without the required cpu flags
    //   - workers running different hypervisor version
    //   - workers in 'quarantined' (draining is allowed — drain just hibernate-then-resume)
    //   - workers with lease_state != 'held'
}
```

Called whenever a new session is dispatched or a remote recovery needs a worker.

### 5.6 `crates/puku-controld/src/api/mod.rs` (extend)

**Accept new states in transitions.**

| Endpoint | Change |
|---|---|
| `POST /v1/sessions` | Already returns session object; now includes `desired_state`, `sla_tier`, `last_snapshot_id`. Reject at creation (HTTP 409), never silently downgrade: `premium_requires_warm` (org ceiling is `cold_only`, including while R0 is open); `premium_requires_hot_pool` (hot pool disabled or unhealthy, §4.4.3a); `warm_unavailable` (no worker advertises `warm_resume_supported = true`). |
| `POST /v1/sessions/{id}/resume` | Accept resuming from `hibernated`, `cold_archived`, `archived` — long poll for cold resume |
| `POST /v1/sessions/{id}/stop` | Sets `desired_state=stopped` BEFORE delegating to worker |
| `GET /v1/sessions/{id}` | Returns `desired_state`, `sla_tier`, `last_snapshot_id`, `snapshot_taken_at`, `fence_state` |
| New: `POST /v1/admin/fence/{host_id}` | Manual fence (operator only) |

### 5.7 `crates/puku-controld/src/archive.rs` (extend)

**Tier transition logic** — see §7.

### 5.8 `crates/puku-workerd/src/session_actor.rs` (extend)

**Remove direct volume access; use `puku-volume`.**

| Current line (approx) | Replace with |
|---|---|
| `let vol = std::path::PathBuf::from("/var/lib/puku/sessions")...` | `let vol = volume.attach(vol_id, self_host).await?` |
| Direct `mkdir` and `bind-mount` | `volume.create()` and `volume.attach()` |
| `fs::write(spec.json, ...)` to local disk | `fs::write(spec.json, ...)` to the **attached volume's mountpoint** |

**Add idle-snapshot listener.**

```rust
// In session_actor's main loop:
let mut idle_rx = guestd::subscribe_idle_signal(&session_id)?;
loop {
    select! {
        _ = idle_rx.recv() => {
            if let Some(manifest) = snapshot.capture_paired(...).await.ok() {
                last_snapshot_id = Some(manifest.id);
                last_snapshot_ts = Some(manifest.ts);
            }
        }
        _ = event_outbox_tail.tick() => { ... existing ... }
    }
}
```

**Add desired_state check.**

```rust
// On VM exit:
if desired_state == "stopped" {
    // expected shutdown — no recovery
    return Ok(Outcome::Stopped);
} else {
    // crash — trigger recovery
    let outcome = controld::report_crash(session_id, last_snapshot_id).await?;
    return Ok(Outcome::Crashed(outcome));
}
```

### 5.9 `crates/puku-guestd/` (extend)

| File | Add |
|---|---|
| `src/heartbeat.rs` | New. Send heartbeat over vsock every 1 s. If no consumer for > 3 s, log warning. |
| `src/idle.rs` | New. Watch puku-cli's control channel; send `idle_for_snapshot` when between tool calls. |
| `src/shutdown.rs` | Extend. On SIGTERM, send `clean_shutdown` message before exit. |
| `src/package_watcher.rs` | **New.** Watch package-manager activity (apt, pip, npm, cargo, go, gem, brew) — see §4.5 below. Writes rows to `installed_packages` via vsock control message. This is what makes `installed_packages` table actually populated, which is what makes environment rebuild possible after disk loss. |

### 5.10 `crates/puku-cloud-proto/` (extend)

Add to `src/lib.rs`:

```rust
pub mod v2 {
    // Wire types for the reliability rebuild.
    pub mod lease;
    pub mod fence;
    pub mod volume;
    pub mod snapshot;
}
```

---

## 6. Chaos tests (CI gate)

**Path:** `tests/chaos/`

Each test is a separate Rust integration test that requires a running cluster. All must pass on every PR.

| Test | File | What it does | Pass criterion |
|---|---|---|---|
| **T1** `kill_vmm` | `kill_vmm.rs` | Send SIGKILL to a running VM's VMM process, but FIRST write a marker file inside the guest and call `fsync` on it (the marker must be `fsync`ed to its backing store before the kill — the test asserts this) | New VM on same host, R2 events tail resumes from `last_snapshot_id`, **zero events lost** (compare event count before vs after), and the **marker file written and `fsync`ed in the guest immediately before the kill is present after recovery** (no rollback past the fsync point). |
| **T2** `guest_hang` | `guest_hang.rs` | `kill -STOP` puku-cli inside the guest; vsock heartbeat stops | Tier-1 recovery kills VMM, restarts from snapshot, agent continues |
| **T3** `crash_loop_eviction` | `crash_loop.rs` | Trigger 3 VMM kills in 10 min | Session evicted to another host after 3rd crash; eviction logged |
| **T4** `host_kernel_panic` | `host_kernel_panic.rs` | `echo c > /proc/sysrq-trigger` on a worker host, but FIRST write a marker file inside the guest and `fsync` it (asserted) | Lease expires within 3 s, fenced, session restarts on another host within 30 s, and the **marker file written and `fsync`ed in the guest immediately before the kill is present after recovery on the new host** (ColdHead for cold sessions; warm sessions restore the disk to the snapshot point and so will not see it — the test runs against a `cold_only` session). |
| **T5** `network_partition` | `network_partition.rs` | `iptables -I OUTPUT -d <controld_ip> -j DROP` on a worker | Lease expires, fenced, session restarts elsewhere; **old host's writes rejected** (verify by writing a marker file from old host — should fail) |
| **T6** `corrupt_snapshot` | `corrupt_snapshot.rs` | Bit-flip the latest manifest's `sha256` column | Restore detects corruption, tries previous-of-3, falls back to cold resume if needed; session continues |
| **T7** `warm_resume_after_kill` | `warm_resume.rs` | SIGKILL the VM during a tool call; agent had said "running pytest" | After warm resume, puku-cli sees a normal `can_use_tool` interrupt for the in-flight tool; the tool is **re-run from scratch** with the same input. The agent's transcript is continuous; the tool call is recorded twice (kill + re-run), with a `recovery: warm_resume_v1` annotation. Warm resume restores the state from before the tool started, so the tool cannot have a "result" — it must re-execute. |
| **T8** `kill_controld` | `kill_controld.rs` | SIGTERM one of N controld instances | Other instances pick up via NOTIFY; **no API 5xx > 1%** in the test window; events still flowing |
| **T9** `no_silent_delete_invariant` | `no_silent_delete.rs` | Run a session, complete it, wait 30 days (mocked) | After 30 days, session is in `cold_archived` (not `archived` — that takes 90 days by default). For archived state, the **R2 archive object** (`archive/<id>/disk.tar.zst`) must still exist with a valid `archive/<id>/disk.sha256`. Transcript, manifest, mem snap, and disk archive are all present. |
| **T10** `expire_lease` | `expire_lease.rs` | Worker stops renewing its lease for 4 s | Sweeper marks lease `suspected` within 3 s. New dispatch refuses to place work on this worker. `fence_state` is `suspected`. |
| **T11** `recover_from_hibernated` | `recover_from_hibernated.rs` | Mark a session `hibernated` with a valid manifest; POST /resume | Within 30 s, session is `running` on a healthy host. RBD volume attached, mem snap restored, agent continues from `last_seq`. |
| **T12** `recover_from_archived` | `recover_from_archived.rs` | Mark a session `archived` with the R2 disk archive present; POST /resume | Worker downloads `archive/<id>/disk.tar.zst`, verifies sha256, unpacks to fresh RBD clone, applies manifest, restores mem snap, session `running`. **RTO is minutes**, not seconds — test polls up to 10 min. |
| **T13** `snapshot_migrated_to_incompatible_host` | `snapshot_migrated_to_incompatible_host.rs` | Session on host A, manifest has `cpu_flags=[avx512]`. Try to recover on host B which lacks avx512 | Restore **fails** with a clear error in the session's event log: "snapshot requires cpu_flags=[avx512] not present on host B (fingerprint mismatch)". No silent failure, no half-restored VM. Operator paged. |
| **T14** `proxy_failover` | `proxy_failover.rs` | Open a WS to `puku-proxy`, run an agent, `kill_controld` on the attached controld mid-`waiting_input` | Client sees exactly one `reconnected` event, no gap, no duplicate events (seq dedup by client), the unanswered question still arrives exactly once |
| **T15** `cold_resume_after_kill` | `cold_resume.rs` | Set session `recovery_mode = cold_only`, install `jq` via apt, run an agent tool call, SIGKILL the VM mid-tool | After cold resume: `installed_packages` still has `(apt, jq)`, RBD head has the file the user created during the run, agent re-reads transcript, in-flight tool re-runs. **No rollback to a snapshot.** |
| **T17** `ceph_pool_loss_restore` | `ceph_pool_loss.rs` | Create files and `apt install jq`; wait one backup interval (mocked); delete the session's RBD image; `POST /resume` | Outcome is `FromArchive`; the files present at the last backup and `(apt, jq)` exist; the event log replays from `last_seq`; loss is ≤ `disk_backup_interval`. |

### 6.1 Test runner contract

All tests must run in a CI workflow:

```yaml
# .github/workflows/chaos.yml
name: chaos
on: [pull_request]
jobs:
  chaos:
    runs-on: [self-hosted, chaos-runner]
    steps:
      - uses: actions/checkout@v4
      - run: cargo build --release --workspace
      - run: docker compose -f deploy/compose.dev.yml up -d
      - run: ./deploy/scripts/prestage-rbd.sh     # Ceph + base image
      - run: cargo test --release --test '*' -- --test-threads=1
        # serial because tests kill hosts
```

A **PR cannot merge** if any chaos test fails.

---

## 7. Tier transitions

**Path:** `crates/puku-controld/src/sweeper.rs::transition_tiers()` (new function).

| From | To | When | Action |
|---|---|---|---|
| `running` / `waiting_input` | `hibernated` | idle > `idle_timeout_s` AND desired_state=`running` | **Warm sessions:** capture paired snapshot, wait for `LocalDurable`, stop VM. **Cold sessions:** guest `sync`, stop VM (no memory snapshot). In both cases RBD stays; set `last_snapshot_id` if one exists; update `sessions.state`. |
| `hibernated` | `cold_archived` | `now() - snapshot_taken_at > cold_after_days` (default 7) | Move RBD to `rbd-cold` pool (EC k=4 m=2) |
| `cold_archived` | `archived` | `now() - snapshot_taken_at > archive_after_days` (default 90) | **Export disk to R2 first** as `archive/<session_id>/disk.tar.zst` (sha256 in `archive/<session_id>/disk.sha256`). Then delete the RBD volume. R2 must have: mem snap, manifest, and now the disk tarball. |
| any of above | `deleted` | **only** explicit `DELETE /v1/sessions/{id}` | Delete RBD (if any) + R2 objects (archive, mem snap, manifest); keep session row + transcript + installed_packages table forever |

**Resume from `hibernated`.** Warm session with a restorable manifest → `Warm`. Cold session, or warm session whose manifest is missing/corrupt/incompatible → `ColdHead` (RBD head, no rollback).

**Archived sessions are fully recoverable.** Resume from `archived` first downloads `archive/<id>/disk.tar.zst` (verified by sha256), unpacks onto a fresh RBD clone, applies manifest, restores mem snap, resumes. Cold resume RTO on archived = minutes (depends on disk size + bandwidth). See `recover_from_archived` test in §6.

Configurable per-org:

```sql
ALTER TABLE quotas ADD COLUMN hibernate_after_minutes int NOT NULL DEFAULT 15;
ALTER TABLE quotas ADD COLUMN cold_after_days int NOT NULL DEFAULT 7;
ALTER TABLE quotas ADD COLUMN archive_after_days int NOT NULL DEFAULT 90;
```

---

## 8. Observability

### 8.1 Metrics (new in `/metrics`)

```
# Lease health
puku_lease_state{host_id,state}                        gauge
puku_lease_expired_total                              counter

# Fence outcomes
puku_fence_total{action,outcome}                       counter
puku_fence_duration_seconds{action}                    histogram

# Recovery tier choice
puku_recovery_total{tier,outcome}                      counter
puku_recovery_duration_seconds{tier}                  histogram

# Snapshot capture
puku_snapshot_capture_total{session_id}                counter  # cardinality: bounded by tier
puku_snapshot_capture_pause_seconds                   histogram
puku_snapshot_verify_total{result}                     counter

# Test-restore
puku_snapshot_test_total{result}                       counter
puku_snapshot_test_duration_seconds                    histogram

# Split-brain attempt (must always be 0)
puku_splitbrain_attempt_total                          counter
```

### 8.2 Sentry tags (new)

```
session_id, host_id, sla_tier, fence_state,
lease_expired, snapshot_manifest_id, snapshot_corrupt,
recovery_outcome
```

### 8.3 Alerts (runbook in `docs/RELIABILITY-RUNBOOK.md`)

| Alert | Condition | Severity |
|---|---|---|
| `LeaseExpiredLong` | `lease.state = suspected` for > 30 s | Page |
| `FenceFailed` | `puku_fence_total{outcome="failed"}` > 0 in 5 min | Page |
| `SnapshotVerifyFailing` | `puku_snapshot_verify_total{result="corrupt"}` > 5 in 1 h | Warn |
| `RecoveryOver30s` | `puku_recovery_duration_seconds{tier="remote",quantile="0.95"}` > 30 | Page |
| `SplitbrainAttempt` | `puku_splitbrain_attempt_total` > 0 (must be impossible) | SEV1 |
| `RPOAtRisk` | `sessions.rpo_at_risk = true` — premium page, standard ticket (set when no `LocalDurable` within `rpo_seconds`, or when the snapshot trigger skipped due to backlog cap, §4.4.2) | Page (premium) / Ticket (standard) |
| `HotPoolUnavailable` | `puku_snap_hot_pool_ok` is false OR `ceph health` reports `puku-snap-hot` `degraded`/`down` for > 60 s — premium SLA broken | Page |
| `SnapshotCompactionFailed` | `puku_snapshot_compaction_total{result="failed"}` > 0 in 1 h (old chain left intact per §4.4.4, but this is a sign the merge job is wedged) | Warn |
| `DiskBackupStale` | `now() - last_disk_backup_at > 2 × disk_backup_interval_s` for any session with a backup chain — a whole-pool-loss restore would lose more than one interval of writes | Warn |

---

## 9. Deployment scripts (extend or add)

### 9.1 New: `deploy/scripts/prestage-rbd.sh`

Stages the Ceph cluster for the cluster operator. Run once per cluster, not per host.

```bash
#!/bin/bash
set -euo pipefail

# 1. Create pools.
ceph osd pool create rbd-base 128 replicated
ceph osd pool create rbd-sessions 96 replicated
ceph osd pool create rbd-cold 64 erasure-code   # k=4 m=2
ceph osd pool create puku-snap-hot 64 replicated
ceph osd pool set puku-snap-hot size 3
ceph osd pool set puku-snap-hot min_size 2
ceph osd pool application enable puku-snap-hot rados

# 2. Import base image from local OCI tarball (built elsewhere).
rbd -p rbd-base import puku-agent-0.1.0.raw puku-agent-0.1.0
rbd -p rbd-base snap create puku-agent-0.1.0@snap --size=...
rbd -p rbd-base snap protect puku-agent-0.1.0@snap

# 3. Create a puku user with restricted caps.
ceph auth add client.puku-cap \
    mon 'allow r' \
    osd 'allow rw pool=rbd-base, allow rw pool=rbd-sessions, allow rw pool=rbd-cold, allow rw pool=puku-snap-hot'

# 4. Distribute ceph.conf to all hosts.
scp /etc/ceph/ceph.conf worker-1:/etc/ceph/
...
```

### 9.22 Extend: `deploy/scripts/preflight.sh`

Append these checks.

| Check | Pass |
|---|---|
| `/usr/bin/rbd` exists | yes |
| `ceph -s` returns `HEALTH_OK` | yes |
| `rbd -p rbd-base ls` includes `puku-agent-*` | yes |
| `/dev/kvm` exists | yes (existing check) |
| `ipmitool` or `redfish` reachable on BMC | yes (only if BMC configured) |
| CPU flags match cluster baseline (`grep -m1 -f` against `/etc/puku/cluster.cpu_flags`) | yes |
| `ceph osd pool get puku-snap-hot size` returns 3 | yes |
| Write, read back and delete a 1 MiB test object in `puku-snap-hot` | yes |
| `puku-workerd` advertises `hot_pool_ok = true` (needed for premium placement) | yes |
| Effective RBD client config has `rbd_cache_writethrough_until_flush = true` (or cache off), and the VM launch args do not set `cache=unsafe` | yes |

### 9.3 Extend: `deploy/systemd/puku-workerd.service`

Append `Environment=PUKU_VOLUME_BACKEND=rbd` and `Environment=PUKU_RBD_*` for the Ceph config.

### 9.4 New: `deploy/scripts/upgrade-cluster.sh`

Order matters:

```
1. Pull new code.
2. Rebuild controld, workerd, puku-guestd.
3. Build new base image (e.g. puku-agent-0.2.0).
4. Import new base into rbd-base, snapshot, protect.
5. Roll controld instances (1 at a time, wait for "online" per worker).
6. Roll workers (1 at a time, each worker drains then restarts).
   - Worker drains → for each session on this worker:
       * warm session: capture a paired snapshot and wait for `Durable` (bounded by `DRAIN_DEADLINE_S`, default 120 s);
       * cold session: guest `sync`, stop VM.
     Mark `desired_state = stopped`, state `hibernated`. The recovery path resumes it on a healthy worker
     (`Warm` if the snapshot is Durable, otherwise `ColdHead`). No session waits for a tool call to finish.
     If a warm snapshot misses the deadline, fall back to cold stop. The disk head is never lost.
   - Worker restarts with new binary.
   - Sessions resume on other workers via the standard hibernated→running path.
7. Set PUKU_AGENT_IMAGE=new tag on controld. (Restart one controld to load.)
```

**Never** roll base image while sessions from the old image are still on workers (they can't warm-resume across a base-image bump without a cold resume first).

---

## 10. Acceptance criteria (the spec the rebuild must meet)

The rebuild is done when **all** the following are true:

| # | Criterion | How to verify |
|---|---|---|
| AC1 | All 5 migrations applied without error | `cargo run --bin puku-controld migrate` succeeds |
| AC1a | R0 hypervisor bake-off complete: pause p50/p95 + restore p50/p95 measured for CH, libkrun/msb, Firecracker, QEMU/KVM at the shipped VM size; primary engine chosen | `bench/R0_REPORT.md` exists with measured numbers; `crates/puku-workerd/Cargo.toml` reflects the chosen engine; `preflight.sh` checks the chosen engine |
| AC2 | `puku-volume`, `puku-leases`, `puku-fence`, `puku-snapshot`, `puku-proxy`, `puku-rebuild` published | `cargo build --workspace` succeeds |
| AC3 | F1 passes (sandbox crash, no host loss) | `cargo test --test kill_vmm` |
| AC4 | F2 passes (guest hang) | `cargo test --test guest_hang` |
| AC5 | F3 passes (crash loop eviction) | `cargo test --test crash_loop` |
| AC6 | F4 passes (host death) | `cargo test --test host_kernel_panic` |
| AC7 | F5 passes (network split) | `cargo test --test network_partition` |
| AC8 | F6 verified (split-brain impossible) | Manual inspection of `fence_log` shows blocklist precedes every cross-host relocate |
| AC9 | F7 passes (corrupt snapshot fallback) | `cargo test --test corrupt_snapshot` |
| AC10 | F8 RPO matches tier, warm AND cold paths | Warm: 60 s RAM loss proven in `cargo test --test warm_resume`. Cold: zero disk loss + full RAM loss proven in `cargo test --test cold_resume`. |
| AC18 | `puku-rebuild` emits a valid shell script from `installed_packages` | `cargo test --test rebuild_from_packages` — script installs the same packages in a fresh base image |
| AC19 | Off-cluster disk backup restores a session after RBD image loss | `cargo test --test ceph_pool_loss_restore` — files present at the last backup and `installed_packages` survive |
| AC11 | F9 RTO < 15 min | Manual drill (out of CI scope) |
| AC12 | F10 (no silent delete) | `cargo test --test no_silent_delete` |
| AC13 | All metrics on `/metrics` | `curl /metrics` shows them all |
| AC14 | Sentry receives new tags | Trigger a fake crash, inspect Sentry event |
| AC15 | Docs in `docs/RELIABILITY-RUNBOOK.md` describe the operator workflow | Manual review |
| AC16 | Chaos CI blocks PRs without passing | Merge a broken PR — CI fails |
| AC17 | `puku-proxy` failover is invisible to clients | `cargo test --test proxy_failover` — WS survives `kill_controld` without a gap or duplicate event |

---

## 11. Phase plan (each phase ends at a chaos test)

| Phase | Deliverable | Chaos gate to next phase |
|---|---|---|
| **R0** Hypervisor bake-off | Bench CH, libkrun/msb, Firecracker, QEMU/KVM on the actual VM sizes we ship. Measure pause p50/p95 for `capture_paired(mode=Diff)` and restore p50/p95 for `verify_latest()`. Pick the primary engine. | Decision recorded in §15.4. Until R0 closes, every org's `quotas.recovery_mode` is forced to `cold_only` (org ceiling = `cold_only`), so even though the per-session default is `warm_allowed`, no session can opt up. |
| **R1** Volume abstraction | `puku-volume` with `Local` + `Rbd` backends. Refactor `session_actor.rs` to use it. Volume still host-pinned. | `kill_vmm` passes (no behavior change). |
| **R2** Leases | `puku-leases`, control-plane sweeper, workerd heartbeat every 1 s, desired_state column. | `kill_vmm` still passes; `expire_lease` passes. |
| **R3** Fencing + remote tier + session proxy | `puku-fence`, Ceph blocklist integration, BMC stub, cross-host recovery. **`puku-proxy`** with `reconnect_token`, replay from `last_seq` (needs recovery and the sweeper to exist). | `host_kernel_panic` + `network_partition` + `proxy_failover` pass. **Manual inspection of `fence_log` proves blocklist precedes every cross-host relocate** (F6). |
| **R4** Warm resume + post-restore refresh | `puku-snapshot` (incremental diff chain, background merge, backlog cap), idle trigger, single per-tier RPO constant, test-restore job. **`puku-guestd/src/restore_refresh.rs`** (clock / machine-id / RNG reset on every restore). Only after R0 picks an engine that actually supports diff memory snapshots. | `warm_resume_after_kill` + `corrupt_snapshot` pass. |
| **R5** Tiered retention + per-tier RPO + environment rebuild | Hibernated/Cold/Archived states, erasure-coded pool, per-org tier. **`puku-guestd/src/package_watcher.rs`** watching `/var/lib/dpkg/status` and pip dist-info. **`puku-rebuild`** CLI. **`disk_backup.rs`** (periodic `rbd export-diff` to R2). | `recover_from_hibernated` + `recover_from_archived` + `cold_resume_after_kill` + `ceph_pool_loss_restore` + `no_silent_delete` pass. |
| **R6** CPU parity + preflight | Worker publishes fingerprint, dispatcher enforces match, `preflight.sh` extends. | `snapshot_migrated_to_incompatible_host` test fails with clear error (we can't recover, but we fail loudly). |

Total: 7 phases × 2–6 weeks each ≈ 6–7 months. R0 is 1–2 weeks on a chaos runner; R4 is the longest phase because the snapshot stack is new.

---

## 12. Honest gaps (carried forward from Notion §7)

These cannot be solved by software alone. The rebuild does not promise to fix them.

1. **True zero-loss memory recovery** is not realistic without Remus/COLO (QEMU/Xen only). We promise 5 s / 60 s RPO for premium / standard warm resume; cold_only sessions lose RAM entirely.
2. **Ceph needs operational skill** — 3 storage nodes, 25 GbE, monitor quorum. Mitigation: `prestage-rbd.sh` automates initial setup; `preflight.sh` validates.
3. **Snapshot features depend on hypervisor version** — libkrun/CH/Firecracker diff snapshot support varies. Verify on the version we ship in CI on every PR.
4. **Live migration** is only worth it for > 50-host fleets. Out of scope for reliability rebuild.
5. **Backups are periodic.** A whole-pool loss costs up to `disk_backup_interval` of disk writes; shorten the interval per tier if that is not acceptable.

---

## 13. File-by-file build order

Build these in this exact order. Each line tells you: **what to create**, **what it depends on**, **how long it takes**, **how you know it's done**.

| # | File | Deps | Time | Done when |
|---|---|---|---|---|
| 1 | `migrations/0027_reliability_states.sql` | — | 1 h | `cargo run migrate` succeeds |
| 2 | `migrations/0028_leases.sql` | 0027 | 30 min | migrate succeeds |
| 3 | `migrations/0029_snapshots.sql` | 0027 | 30 min | migrate succeeds |
| 4 | `migrations/0030_fence_log.sql` | 0027 | 30 min | migrate succeeds |
| 5 | `migrations/0031_packages.sql` | 0027 | 30 min | migrate succeeds |
| 6 | `crates/puku-volume/Cargo.toml` + `src/lib.rs` + `src/traits.rs` + `src/types.rs` + `src/error.rs` | — | 4 h | `cargo build -p puku-volume` |
| 7 | `crates/puku-volume/src/local.rs` | 6 | 4 h | unit tests pass: create → attach → write → detach → attach → read |
| 8 | `crates/puku-volume/src/rbd.rs` | 6 | 1 day | unit tests pass against a real Ceph dev cluster |
| 9 | `crates/puku-volume/src/fence.rs` | 6 | 4 h | unit tests: blocklist → write fails, unfence → write succeeds |
| 10 | `crates/puku-leases/Cargo.toml` + `src/lib.rs` + `src/heartbeat.rs` + `src/sweeper.rs` | 2 | 1 day | unit + integration tests pass |
| 11 | `crates/puku-fence/Cargo.toml` + `src/lib.rs` + `src/ceph.rs` + `src/ipmi.rs` + `src/redfish.rs` + `src/audit.rs` | 2, 4 | 1 day | unit tests against real Ceph + mock BMC |
| 12 | `crates/puku-snapshot/Cargo.toml` + `src/lib.rs` + `src/manifest.rs` | 3 | 4 h | `cargo build -p puku-snapshot` |
| 13 | `crates/puku-snapshot/src/capture.rs` | 9, 11, 12 | 2 days | integration test: capture + verify SHA-256 |
| 14 | `crates/puku-snapshot/src/restore.rs` | 13 | 1 day | integration test: warm/cold/base fallback |
| 15 | `crates/puku-snapshot/src/retention.rs` | 13 | 4 h | unit test: reaper deletes children first; old "keep last 3" wording is replaced — that was the *chain* cap, which the new compaction rules supersede |
| 15a | `crates/puku-snapshot/src/compaction.rs` | 13, 14, 8 | 1 day | integration test: depth-8 chain triggers merge, B' is byte-identical to applying diffs in order, cut-over re-parents in-transit captures, failure leaves old chain intact |
| 16 | `crates/puku-snapshot/src/test_restore.rs` | 14 | 4 h | unit test: bit-flip → ok=false written |
| 17 | `crates/puku-snapshot/src/idle_signal.rs` | — | 4 h | unit test: signal received → capture called |
| 18 | `crates/puku-cloud-proto/src/v2/lease.rs` + `fence.rs` + `volume.rs` + `snapshot.rs` | — | 1 day | `cargo build -p puku-cloud-proto` |
| 19 | `crates/puku-controld/src/leases.rs` | 10, 18 | 4 h | unit tests against Postgres |
| 20 | `crates/puku-controld/src/fence.rs` | 11, 18 | 4 h | unit tests: writes to fence_log |
| 21 | `crates/puku-controld/src/recovery.rs` | 19, 20, 9, 14 | 2 days | unit tests: tier decision matrix |
| 22 | `crates/puku-controld/src/sweeper.rs` | 19, 20, 21, 15, 16 | 1 day | unit tests: every job ticks |
| 23 | `crates/puku-controld/src/scheduler.rs` (extend) | 18 | 4 h | unit tests: fingerprint match works |
| 24 | `crates/puku-controld/src/archive.rs` (extend) | 22 | 4 h | unit tests: tier transitions |
| 25 | `crates/puku-controld/src/api/mod.rs` (extend) | 23, 24 | 4 h | integration tests: new endpoints + new state fields |
| 26 | `crates/puku-workerd/src/session_actor.rs` (refactor to use puku-volume) | 6, 7, 8 | 1 day | all existing session tests pass |
| 27 | `crates/puku-workerd/src/session_actor.rs` (add idle snapshot) | 17, 26 | 1 day | integration test: idle signal triggers capture |
| 28 | `crates/puku-workerd/src/session_actor.rs` (add desired_state check) | 1, 26 | 4 h | unit test: expected shutdown vs crash distinguished |
| 29 | `crates/puku-workerd/src/main.rs` (add lease heartbeat task) | 10 | 4 h | integration test: heartbeat fires every 1 s |
| 30 | `crates/puku-guestd/src/heartbeat.rs` | — | 4 h | unit test: heartbeat emitted every 1 s |
| 31 | `crates/puku-guestd/src/idle.rs` | — | 4 h | unit test: signal sent when between tool calls |
| 32 | `crates/puku-guestd/src/shutdown.rs` (extend) | — | 4 h | unit test: clean_shutdown sent on SIGTERM |
| 33 | `crates/puku-observability/src/scrub.rs` (extend tags) | — | 4 h | unit test: new tags emitted |
| 34 | `tests/chaos/T1_kill_vmm.rs` | 26 | 4 h | passes in CI |
| 35 | `tests/chaos/T2_guest_hang.rs` | 30, 32 | 4 h | passes in CI |
| 36 | `tests/chaos/T3_crash_loop.rs` | 26, 23 | 4 h | passes in CI |
| 37 | `tests/chaos/T4_host_kernel_panic.rs` | 11, 21 | 4 h | passes in CI |
| 38 | `tests/chaos/T5_network_partition.rs` | 11, 21 | 4 h | passes in CI |
| 39 | `tests/chaos/T6_corrupt_snapshot.rs` | 14 | 4 h | passes in CI |
| 40 | `tests/chaos/T7_warm_resume.rs` | 14, 27 | 4 h | passes in CI |
| 41 | `tests/chaos/T8_kill_controld.rs` | 22 | 4 h | passes in CI |
| 42 | `tests/chaos/T9_no_silent_delete.rs` | 24 | 4 h | passes in CI |
| 43 | `deploy/scripts/prestage-rbd.sh` | — | 1 day | manual: Ceph ready, base image imported |
| 44 | `deploy/scripts/preflight.sh` (extend) | 43 | 4 h | unit test: new checks pass |
| 45 | `deploy/systemd/puku-workerd.service` (extend) | 8, 10 | 1 h | manual: worker starts with rbd backend |
| 46 | `deploy/scripts/upgrade-cluster.sh` | 43–45 | 4 h | manual: rolling upgrade works |
| 47 | `.github/workflows/chaos.yml` | 34–42 | 4 h | manual: PR fails on broken chaos test |
| 48 | `docs/RELIABILITY-RUNBOOK.md` | 19–22 | 1 day | manual review |
| 49 | `crates/puku-guestd/src/package_watcher.rs` | — | 1 day | unit + integration test: `apt install jq` writes a row within 5 s |
| 50 | `crates/puku-guestd/src/restore_refresh.rs` | — | 4 h | unit test: clock / machine-id / entropy pool reset on every restore |
| 51 | `crates/puku-rebuild/src/main.rs` | 49 | 1 day | integration test: `puku-rebuild` emits a valid shell script from a populated `installed_packages` row set |
| 52 | `crates/puku-proxy/src/main.rs` | 22 | 3 days | chaos test `proxy_failover` passes: WS survives controld failover without a gap or duplicate |
| 53 | `tests/chaos/T10_expire_lease.rs` | 10 | 4 h | passes in CI |
| 54 | `tests/chaos/T11_recover_from_hibernated.rs` | 14, 22 | 4 h | passes in CI |
| 55 | `tests/chaos/T12_recover_from_archived.rs` | 14, 22 | 4 h | passes in CI (may take minutes; CI timeout = 15 min) |
| 56 | `tests/chaos/T13_snapshot_migrated_to_incompatible_host.rs` | 14, 23 | 4 h | passes in CI with expected error message |
| 57 | `tests/chaos/T14_proxy_failover.rs` | 52, 41 | 4 h | passes in CI |
| 58 | `tests/chaos/T15_cold_resume.rs` | 14, 49 | 4 h | passes in CI: installed packages preserved, RBD head intact, no rollback |
| 59 | `tests/chaos/T16_rebuild_from_packages.rs` | 51 | 4 h | passes in CI: fresh base + replayed installs = same env |
| 60 | `migrations/0032_disk_backups.sql` | 0027 | 30 min | migrate succeeds |
| 61 | `crates/puku-snapshot/src/disk_backup.rs` | 8, 60 | 2 days | integration test: backup, empty-diff skip, restore from chain |
| 62 | `tests/chaos/T17_ceph_pool_loss_restore.rs` | 14, 61 | 4 h | passes in CI |

**Sum:** **~6–7 months** of focused work for an experienced engineer who knows Ceph, Postgres, and Rust. The phase plan in §11 (7 phases × 2–6 weeks, R0–R6) is the honest number; this build order (62 files) is the breakdown of those phases.

---

## 14. Quick reference: what lives where

| Concept | File |
|---|---|
| Volume abstraction | `crates/puku-volume/src/traits.rs` |
| RBD backend | `crates/puku-volume/src/rbd.rs` |
| Lease service | `crates/puku-leases/src/lib.rs` |
| Fence | `crates/puku-fence/src/lib.rs` |
| Snapshot capture (with diff memory) | `crates/puku-snapshot/src/capture.rs` |
| Snapshot restore | `crates/puku-snapshot/src/restore.rs` |
| Snapshot manifest SHA-256 | `crates/puku-snapshot/src/manifest.rs` |
| Snapshot retention (newest-first reaper, post-compaction) | `crates/puku-snapshot/src/retention.rs` |
| Snapshot compaction (overlay merge, depth≥8 or RAM×0.5) | `crates/puku-snapshot/src/compaction.rs` |
| Snapshot triggers + RPO constant (`effective_recovery_mode`, `rpo_seconds`) | `crates/puku-snapshot/src/triggers.rs` |
| Diff-chain bookkeeping (current base, next-to-compact) | `crates/puku-snapshot/src/chain.rs` |
| Off-cluster disk backup (`rbd export`/`export-diff` → R2) | `crates/puku-snapshot/src/disk_backup.rs` |
| Test-restore job | `crates/puku-snapshot/src/test_restore.rs` |
| Idle signal receiver | `crates/puku-snapshot/src/idle_signal.rs` |
| Package watcher (fills `installed_packages`) | `crates/puku-guestd/src/package_watcher.rs` |
| Post-restore refresh (clock/RNG/machine-id) | `crates/puku-guestd/src/restore_refresh.rs` |
| Environment rebuild tool | `crates/puku-rebuild/src/main.rs` |
| Recovery logic (controld) | `crates/puku-controld/src/recovery.rs` |
| Lease sweeper (controld) | `crates/puku-controld/src/sweeper.rs` |
| Tier transitions (controld) | `crates/puku-controld/src/archive.rs` |
| Session proxy | `crates/puku-proxy/src/main.rs` |
| Guest heartbeat | `crates/puku-guestd/src/heartbeat.rs` |
| Guest idle signal sender | `crates/puku-guestd/src/idle.rs` |
| Worker heartbeat | `crates/puku-workerd/src/main.rs` (new task) |
| Session actor refactor | `crates/puku-workerd/src/session_actor.rs` |
| Wire types (v2) | `crates/puku-cloud-proto/src/v2/` |
| Database schema | `migrations/0027_*` through `0032_*` |
| Ceph staging | `deploy/scripts/prestage-rbd.sh` |
| Preflight checks | `deploy/scripts/preflight.sh` |
| Upgrade order | `deploy/scripts/upgrade-cluster.sh` |
| Chaos CI | `.github/workflows/chaos.yml` |
| Operator runbook | `docs/RELIABILITY-RUNBOOK.md` |
| Chaos tests (T1–T17, plus the rebuild-from-packages T16) | `tests/chaos/` |

---

## 15. Final summary table

The whole rebuild, on one page.

| Axis | Decision | Why |
|---|---|---|
| Disk durability | **Ceph RBD** with 3 replicas | Stateless workers; any host can attach any volume |
| Disk crash consistency | **Journalled filesystem** (ext4/XFS) + RBD snapshot | Self-repairing; msb/CH requires it |
| Host death detection | **Leases** with 1 s heartbeat + 3 s expiry sweeper | 10–100× faster than TCP timeout |
| Split-brain prevention | **Fencing** (Ceph blocklist + BMC power cycle) before any failover | Old host loses I/O before new host gets it |
| Memory durability | **Co-issued disk+mem snapshot** (diff against base), chain collapse at 3, idle + forced triggers | RPO per tier: 5 s premium / 60 s standard / 300 s best-effort |
| Recovery model | **Two tiers**: local (< 2 s) on NVMe cache, remote (< 30 s) via Ceph + R2 | Different code paths, different RTOs |
| Recovery mode | **`warm_allowed` (default) or `cold_only` (per session opt-down)** | Warm = preserved RAM, disk rolled back to snapshot point. Cold = zero disk loss, full RAM loss. Default matches the design goal of preserving running programs. |
| Tier retention | **running → hibernated → cold_archived → archived → deleted** (only on request). Archived exports disk to R2 first. | CPU/RAM expensive, disk cheap; archive is fully recoverable |
| Package inventory | **`installed_packages` filled by in-guest watcher** (fanotify + reconcile) | Enables environment rebuild if disk is also lost (`puku-rebuild`) |
| Verified in CI | **16 chaos tests** as PR gate (T1–T16 in §6) | Recovery that is never tested doesn't work |
| CPU/hypervisor portability | **Fingerprint match** at dispatch + preflight check | Snapshots can't restore across incompatible hosts (T13 enforces this) |
| Network identity | **Stable virtual IP/MAC** per session | Reconnect after failover keeps network continuity |
| Client reconnect | **Session proxy** (§15.1) with `reconnect_token`; client retries once and the proxy replays from `last_seq` | Failover is invisible to the client |
| Post-restore refresh | **Clock + RNG seed + machine-id** rewritten on every restore (§15.2) | Avoid duplicate-entropy / clock-jump issues |
| Hypervisor | **Deferred to Phase R0** (§15.4) | Upstream CH is unverified for diff memory; Firecracker is verified. We do not pick on an unverified claim. Until R0 closes, all sessions are `cold_only`; warm is opt-in per org only after R0 picks a verified engine. |
| Live migration | **Deferred to Phase R7** (§15.3) | Only worth it for > 50-host fleets; not in reliability scope |
| Data deletion | **Never silent**; explicit request + audit | Notion §Q2 promise |
| Out of scope | Multi-region, warm-pool, R2 lifecycle, RPO tier optimization | Deferred to scalability / time / storage docs |

### 15.1 Session proxy (new: `crates/puku-proxy/`)

**Purpose.** Make failover invisible to the client. Without this, every failover would tear down the client's WebSocket, force a reconnect, and risk dropping the `waiting_input` state mid-question.

**Wire contract:**

```
Client → Proxy: WebSocket connect to PUKU_CLOUD_URL/ws/sessions/<id>?token=<reconnect_token_or_api_key>
Proxy → Client: hello{last_seq: <int>, instance_id: <str>, recovery_choice: local|remote}
... events stream (same as today's /v1/sessions/{id}/attach) ...

(Failover happens on the backend; the proxy absorbs the cutover.)

Client → Proxy: hello{last_seq: 1234, reconnect_token: <previous>}   (on reconnect)
Proxy → Client: replay events 1234..N from Postgres, then live
```

**The proxy is stateful per session** (it holds the `last_seq` cursor and the `pending_question` state) but stateless overall (no DB writes; reads only). One proxy instance can hold thousands of sessions in memory; horizontal scale is just N proxy instances behind a load balancer that hashes on `session_id`.

**Acceptance criteria:**

| Test | Expected |
|---|---|
| Open WS, normal stream, force a `kill_controld` on the attached controld | Client sees a single `reconnected` event, no gap, no duplicate events (seq dedup by client) |
| Open WS, agent asks question, force failover | Client sees the question arrive exactly once; answer still routes correctly |
| Two clients attach to the same session | Both see the same events; one disconnects, the other continues unaffected |

### 15.2 Post-restore refresh (in `puku-guestd/src/restore_refresh.rs`)

On every restore (warm or cold), the guestd runs:

```rust
// Reset clock (avoids jumps if the host was paused for minutes during a slow failover).
let _ = Command::new("sh").args(["-c", "date -s @$(date +%s)"]).status()?;

// Reset machine-id so the guest isn't recognizable as a previous instance.
let _ = Command::new("sh").args(["-c", "echo -n > /etc/machine-id && systemd-machine-id-setup"]).status()?;

// Re-seed the kernel entropy pool from hardware RNG (already exists, just force a refresh).
let _ = Command::new("sh").args(["-c", "dd if=/dev/urandom of=/dev/random bs=1 count=64 2>/dev/null"]).status()?;
```

These are cheap (sub-second) and happen before `puku-runner` starts. Documented in `puku-guestd/README.md`.

### 15.3 Live migration (Phase R7, deferred)

**Decision.** Out of scope for the reliability rebuild. Justification: live migration is for **planned** moves (maintenance, rebalancing), not crash recovery. Planned moves are handled by `POST /v1/workers/<id>/drain`, which captures a paired snapshot for every session on the worker, marks them `hibernated`, and lets the standard recovery path resume them on a healthy host. AI sessions can run for hours, so "wait for them to finish" is not a viable drain policy. The complexity of pre-copy live migration is not justified at < 50 hosts.

**Engine-agnostic.** This decision does NOT depend on the R0 hypervisor bake-off (§15.4). Whatever engine wins R0, live migration is still deferred: it is a separate phase that adds its own API surface to whichever engine supports it.

**When to revisit.** When the fleet exceeds 50 hosts OR when an operator needs to move a workload without re-running it. At that point, add Phase R7 with the live-migration path on the chosen engine; no breaking change to the current rebuild.

### 15.4 Hypervisor decision (deferred to Phase R0 — Hypervisor Bake-Off)

**Status: unresolved.** Upstream Cloud Hypervisor is unverified for diff memory snapshots (§4.4.2a); Firecracker is verified; libkrun/msb is developer preview. **We do not pick a primary engine on the strength of an unverified claim.**

**What this means in practice.** Until Phase R0 (§11) produces measured numbers:
- Per-org `quotas.recovery_mode` is forced to `cold_only` (the org ceiling), so even though the per-session default is `warm_allowed`, no session can actually be warm. The operator lifts the ceiling only after R0 closes.
- Workers are free to run any of {CH, libkrun/msb, Firecracker, QEMU/KVM}; `puku-workerd` advertises engine capabilities honestly.
- Dispatcher routes `warm_allowed` sessions only to workers that advertise `warm_resume_supported = true` AND have been R0-validated.
- `cold_only` sessions can land on any worker; cold resume does not need a diff-snapshot API.

**Why deferral is safer than picking wrong.** If the primary engine does not actually support what we claim, every `warm_allowed` session is at risk of failing its promise silently. The fix is to measure, then pick. R0 is 1–2 weeks of work on a chaos runner; it blocks the warm-resume ship, not the cold-resume ship.

**Decision table (deferred until R0 results):**

| Criterion | Cloud Hypervisor | libkrun (msb) | Firecracker | QEMU/KVM |
|---|---|---|---|---|
| Diff memory snapshot | **Unverified upstream** (§4.4.2a) | Developer preview | **Verified upstream** | Native |
| Live migration | Postcopy (upstream v53) | None | None | Postcopy |
| Boot time | ~100 ms | ~44 ms | ~50 ms | ~500 ms |
| CPU feature portability | High (KVM) | High | Highest | Highest |
| Stability for production | High (Linux Foundation) | High (Red Hat) | High (AWS) | Highest |
| KVM required | Yes | Yes | Yes | Yes |
| **Verdict before R0** | Candidate | Fallback (already shipped) | Candidate | Last resort |

R0 fills the "Diff memory snapshot" row with measured numbers on the exact engine versions that will ship. The chosen engine then gets its row in `crates/puku-workerd/Cargo.toml` and its preflight check in `deploy/scripts/preflight.sh`. Nothing in §0–§14 changes when R0 closes.

---

**This document is the spec. Build it in the order of §13. Validate against §10. Ship when every chaos test in §6 passes on every PR.**