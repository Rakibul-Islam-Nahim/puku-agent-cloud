# Puku Agent Cloud — Build Plan

> **This file is the work tracker.** It is a navigation aid for the canonical spec at `docs/RELIABILITY-REBUILD.md` (RSD). It is not the spec — open the RSD when you need detail on *what* to build. This file tells you *where you are*, *what is done*, *what is in progress*, and *what to do next*.
>
> **Audience.** The engineer (or AI) picking up the rebuild. You should be able to read this file in five minutes and know exactly which file to open next, why, and how you will know you are done.

---

## 1. One-screen status

| Axis | Count | Where in RSD |
|---|---|---|
| Migrations to add | 6 (`0027`–`0032`) | RSD §3 |
| New crates to create | 6 (`puku-volume`, `puku-leases`, `puku-fence`, `puku-snapshot`, `puku-proxy`, `puku-rebuild`) | RSD §2 |
| Phases | 7 (R0–R6) | RSD §11 |
| Chaos tests | 17 (T1–T17) | RSD §6 |
| Acceptance criteria | 19 (AC1–AC19) | RSD §10 |
| Files in the build order | 62 (62 rows in §13) | RSD §13 |
| Honest gaps carried forward | 5 | RSD §12 |
| Deferred follow-on docs | 3 (scalability, time, storage) | RSD §0.2 |

**Effort envelope.** ~6–7 months of focused work for an engineer who knows Ceph, Postgres, and Rust (RSD §13 footer). R0 (engine bake-off) is the gating decision for warm resume; everything else is implementation.

---

## 2. Spec is the source of truth — what's been written

The RSD is the deliverable. This section records *what has been decided in it* so you don't re-derive it. Every item below is **done** in the sense that the design is committed; nothing here means the code is written.

### 2.1 Core promise (RSD §0.1)

| Property | Guarantee | What makes it true |
|---|---|---|
| Disk data (cold resume) | **Zero loss** of `fsync`ed data; cold resume = RBD head, no rollback | virtio-blk honors flush; `rbd_cache_writethrough_until_flush = true` (or off); `preflight.sh` enforces both |
| Disk data (warm resume) | At most N seconds of disk rollback (N = snapshot age) | Restore disk + memory from same paired snapshot; documented cost |
| RAM | 60 s standard, 5 s premium | `rpo_seconds(tier)` is the only constant (`triggers.rs`); conditional on engine + hot pool |
| Conversation | Always recoverable | Transcript is permanent in Postgres; `session_events` partitioned log |
| Split-brain | Impossible | Fence (Ceph blocklist + BMC) always before any cross-host relocate |
| RTO | < 2 s local / < 30 s host-loss / < 15 min region-loss | See F1–F9 in RSD §1 |
| Whole-Ceph-pool loss | ≤ `disk_backup_interval` of writes (default 1 h) | Off-cluster backup to R2 (RSD §4.7) |
| Recovery mode | `warm_allowed` (default) or `cold_only` (per-session opt-down) | Per-org ceiling, per-session row, DB CHECK enforces best-effort is always cold |

### 2.2 Failure-mode coverage (RSD §1)

10 failure modes (F1–F10) — every one has a detection, action, RTO, and a chaos test name. F6 (split-brain) is verified by inspecting `fence_log` for blocklist-before-relocate ordering. F9 (region loss) is a manual drill.

### 2.3 Snapshot status model (RSD §4.4.1)

```
Pending       → visibility-only, NEVER restorable
LocalDurable  → fsynced on origin NVMe, local restore only
Durable       → verified off-host copy (RADOS hot pool, or R2 when off)
Corrupt       → sha256 mismatch, never restorable
```

This 3-state (plus Corrupt) replaces the old binary "durable / not". It is load-bearing for: the restore ladder (RSD §1.1), the local-vs-remote rule in §4.4.3, the API 409 codes (`premium_requires_warm` / `premium_requires_hot_pool` / `warm_unavailable`), and the test pass criteria T1/T4.

### 2.4 Compaction mechanic (RSD §4.4.4)

- Diffs are sparse page maps. Merge is a **page overlay by offset, newest wins** — not byte concatenation.
- Trigger: `MERGE_DEPTH = 8` or `MERGE_THRESHOLD_BYTES = RAM × 0.5`.
- B' is written through the normal capture path (`LocalDurable` → `Durable`); only after `B'` is `Durable` is the cut-over transaction run.
- Cut-over: re-parent captures taken during the merge onto B', then `DELETE` from the old chain **newest first** (children before parents). The `parent_manifest_id` FK with `ON DELETE RESTRICT` enforces this at the DB level.
- Failure: old chain untouched, alert `SnapshotCompactionFailed` fires.
- Invariant: at no instant is any `Durable` manifest unrestorable; capture is never blocked by compaction.

### 2.5 Hot snapshot pool (RSD §4.4.3a)

| | |
|---|---|
| Pool | `puku-snap-hot` |
| Replication | `size 3`, `min_size 2`, `application rados` |
| What it stores | Current base + diffs after it, for every warm session |
| Eviction | Only after (a) not referenced and (b) `cold_copied_at` set |
| Premium gate | `POST /v1/sessions` returns `409 premium_requires_hot_pool` when disabled or unhealthy |
| Fallback | Captures still complete as `LocalDurable`; remote restore uses R2; `HotPoolUnavailable` alert fires; premium SLA broken until fixed |

### 2.6 Off-cluster disk backup (RSD §4.7)

- `rbd export` (full) or `rbd export-diff --from-snap` (incremental) to R2.
- Empty diffs skipped (idle sessions don't waste R2 IO).
- Compaction at 24 diffs OR on first backup after `archived`.
- Restore = `rbd import` (full) + `rbd import-diff` (chain), sha256 verified at every step.
- `disk_backup_interval_s` defaults to 3600; `last_disk_backup_at` on `sessions`; `DiskBackupStale` alert at 2× interval.

### 2.7 Recovery mode gating (RSD §4.4.3)

`effective_recovery_mode()` is the **only** function that decides warm vs cold. Inputs: `sla_tier`, `session_pref`, `org_ceiling`, `worker_warm_ok`. Output is monotonic (sessions can only lower it). DB CHECK enforces `best_effort` is always `cold_only`.

### 2.8 Drain semantics (RSD §9.4)

Drain deadline is bounded: `DRAIN_DEADLINE_S = 120 s` (default). A warm session captures paired snapshot, waits for `LocalDurable`, then stops. A cold session `sync`s and stops. If a warm snapshot misses the deadline, the drain falls back to cold stop. **The disk head is never lost** during drain.

### 2.9 Cache-mode correctness (RSD §0.1 disk row + §9.2)

The cold-resume zero-loss promise requires:
- virtio-blk device honors guest flush (no `cache=unsafe`)
- RBD client cache runs writethrough until the first flush (`rbd_cache_writethrough_until_flush = true`) or is disabled

`preflight.sh` enforces both. Any other setting silently breaks the promise regardless of the code.

### 2.10 API surface additions (RSD §5.6)

- `POST /v1/sessions` returns new fields; rejects with **409, never silent downgrade**: `premium_requires_warm`, `premium_requires_hot_pool`, `warm_unavailable`.
- `POST /v1/sessions/{id}/resume` accepts `hibernated`, `cold_archived`, `archived`; long-polls for cold resume.
- `POST /v1/sessions/{id}/stop` sets `desired_state=stopped` **before** delegating.
- `GET /v1/sessions/{id}` returns `desired_state`, `sla_tier`, `last_snapshot_id`, `snapshot_taken_at`, `fence_state`.
- New: `POST /v1/admin/fence/{host_id}` (operator-only).

### 2.11 Tier transitions (RSD §7)

```
running / waiting_input  → hibernated   (idle > idle_timeout_s, desired=running)
                              warm: capture + wait LocalDurable + stop
                              cold: sync + stop
hibernated               → cold_archived (now - snapshot_taken_at > cold_after_days)
                              move RBD to rbd-cold (EC k=4 m=2)
cold_archived            → archived      (now - snapshot_taken_at > archive_after_days)
                              export disk to R2 first, sha256 in R2; delete RBD
any                      → deleted       (ONLY on explicit DELETE; transcript kept forever)
```

Per-org configurable: `hibernate_after_minutes=15`, `cold_after_days=7`, `archive_after_days=90`.

### 2.12 Observability (RSD §8)

Metrics: `puku_lease_state`, `puku_fence_total`, `puku_recovery_total`, `puku_snapshot_capture_total`, `puku_snapshot_test_total`, `puku_splitbrain_attempt_total`, **plus** `puku_snapshot_backlog_skip_total`, `puku_durability_lag_seconds{quantile}`.

Sentry tags: `session_id, host_id, sla_tier, fence_state, lease_expired, snapshot_manifest_id, snapshot_corrupt, recovery_outcome`.

Alerts (RSD §8.3, **9 rows**): `LeaseExpiredLong`, `FenceFailed`, `SnapshotVerifyFailing`, `RecoveryOver30s`, `SplitbrainAttempt` (SEV1), `RPOAtRisk` (page premium / ticket standard), `HotPoolUnavailable`, `SnapshotCompactionFailed`, `DiskBackupStale`.

---

## 3. What's been done

| Item | Where | Status |
|---|---|---|
| RSD written and reviewed (2 rounds of issues) | `docs/RELIABILITY-REBUILD.md` (~1790 lines) | **Done** |
| Snapshot status model (3-state + Corrupt) | RSD §4.4.1 | **Done in spec** |
| Compaction mechanic (overlay merge, re-parenting, FK RESTRICT) | RSD §4.4.4 | **Done in spec** |
| Hot pool design (`puku-snap-hot`, 3 replicas, premium gate) | RSD §4.4.3a | **Done in spec** |
| Off-cluster disk backup design (full + diff to R2, 24-diff compaction) | RSD §4.7 | **Done in spec** |
| Recovery-mode gating function `effective_recovery_mode()` | RSD §4.4.3 | **Done in spec** |
| `besteffort_is_cold` DB CHECK constraint | RSD §3.1 | **Done in spec** |
| `rpo_at_risk` column + cron job + alert | RSD §3.1, §4.4.3, §8.3 | **Done in spec** |
| 3-stage Status (Pending / LocalDurable / Durable) trait split | RSD §4.4.1 | **Done in spec** |
| `RestoreOutcome` variants (Warm, ColdHead, FromArchive, Rebuilt, Failed) | RSD §1.1, §4.4.1 | **Done in spec** |
| `migrations/0027_reliability_states.sql` (full text) | RSD §3.1 | **Done in spec** |
| `migrations/0028_leases.sql` (full text) | RSD §3.2 | **Done in spec** |
| `migrations/0029_snapshots.sql` (full text, with status CHECK + parent FK RESTRICT) | RSD §3.3 | **Done in spec** |
| `migrations/0030_fence_log.sql` (full text) | RSD §3.4 | **Done in spec** |
| `migrations/0031_packages.sql` (full text) | RSD §3.5 | **Done in spec** |
| `migrations/0032_disk_backups.sql` (full text) | RSD §3.6 | **Done in spec** |
| `crates/puku-volume/` trait spec + 4 files + RBD cookbook | RSD §4.1 | **Done in spec** |
| `crates/puku-leases/` trait spec + sweeper contract | RSD §4.2 | **Done in spec** |
| `crates/puku-fence/` trait spec + audit, hard rule on fence-before-attach | RSD §4.3 | **Done in spec** |
| `crates/puku-snapshot/` trait spec, capture sequence, 8-file layout | RSD §4.4 | **Done in spec** |
| `crates/puku-proxy/` wire contract | RSD §15.1 | **Done in spec** |
| `crates/puku-rebuild/` CLI design | RSD §4.6 | **Done in spec** |
| `puku-guestd/src/package_watcher.rs` (inotify on dpkg, pip dist-info, npm cache) | RSD §4.5 | **Done in spec** |
| `puku-guestd/src/restore_refresh.rs` (clock / machine-id / RNG) | RSD §15.2 | **Done in spec** |
| Recovery module (`crates/puku-controld/src/recovery.rs`) | RSD §5.1 | **Done in spec** |
| Lease + fence + sweeper + scheduler + archive + api extensions in controld | RSD §5.2–§5.7 | **Done in spec** |
| `session_actor.rs` refactor outline | RSD §5.8 | **Done in spec** |
| `puku-guestd/` extensions (heartbeat, idle, shutdown) | RSD §5.9 | **Done in spec** |
| `puku-cloud-proto/src/v2/` wire types | RSD §5.10 | **Done in spec** |
| `prestage-rbd.sh` (pools + caps + base image import) | RSD §9.1 | **Done in spec** |
| `preflight.sh` extensions (hot pool, cache mode, fingerprint, BMC) | RSD §9.2 | **Done in spec** |
| `upgrade-cluster.sh` (rolling upgrade with drain bounded by 120 s) | RSD §9.4 | **Done in spec** |
| 17 chaos tests, all named with pass criteria | RSD §6 | **Done in spec** |
| 19 acceptance criteria (AC1–AC19) | RSD §10 | **Done in spec** |
| 7-phase plan (R0–R6) with chaos gates | RSD §11 | **Done in spec** |
| 5 honest gaps | RSD §12 | **Done in spec** |
| 62-file build order | RSD §13 | **Done in spec** |
| Quick reference §14 | RSD §14 | **Done in spec** |
| Final summary table §15 | RSD §15 | **Done in spec** |

**No code yet.** The spec is the design; the crates do not exist on disk, the migrations are not in `migrations/`, and no chaos test has been run.

---

## 4. What's in progress — branch `mahi`

The table in §6 predates the code: most of the crates it lists now exist. This section is the truthful state on `mahi`; "tested" means a test exercises it against the real thing named, not a mock.

| Area | State | Proof |
|---|---|---|
| RBD volumes (`puku-volume/src/rbd.rs`) | Real: clone, `map --exclusive`, unmap, snapshot, watcher fencing; every `rbd`/`ceph` failure surfaces | `tests/real_ceph.rs` against MicroCeph (`PUKU_TEST_CEPH=1`), plus scripted unit tests |
| Fencing (`puku-fence`) | Per-volume Ceph blocklist with audit; a failed fence fails the call | Unit tests; real blocklist proven by `real_ceph.rs`. BMC (IPMI/Redfish) not exercised: needs hardware |
| Recovery ordering (`controld/recovery.rs`) | Fence before any remote restore; failed fence stops recovery | Unit tests |
| Host leases (`puku-leases`, `controld/leases.rs`, workerlink) | Worker sends `LeaseRenew` every 1 s; controld takes over on register (new generation), renews, expires on disconnect. Worker never stops its own VMs | Unit tests + controld integration tests on Postgres |
| Lease sweeper | One instance at a time (advisory lock). Suspect at 3 s TTL, dead 15 s later, mass-loss hold above 30 % of 3+ hosts | Unit tests + Postgres integration tests (incl. lock handover) |
| Session disks on RBD (`workerd/volumes.rs`, `PUKU_RBD_POOL`) | Each session gets an image, mapped `--exclusive`, ext4 on first use, mounted at `sessions/<id>/disk` while it runs here and released when it stops; reaping deletes the image. Worker advertises `shared_volumes` | Scripted unit tests + two real-Ceph tests: the disk follows a session between hosts; a crashed holder blocks the next host until fenced, then its synced data is there |
| Moving a shared-disk session (`controld/sharedvol.rs`, dispatch) | Home connected → goes home, no fence. Home declared dead (lease released) → fence home off the disk, then any shared-volume worker. Home only away → waits (never fences a live host). Fence fails → stays queued | Postgres integration tests with a recording fence |
| Machine disks on RBD (`workerd/volumes.rs` `MachineDisks`, migration 0034) | A machine's whole state directory (volume + kept root disk, so installed packages too) is one RBD image, open only while a boot, restore or capture needs it. Home dead → fenced, then boots on any shared-disk worker with its disk (no snapshot). A copy-cleanup never deletes the shared image; only an explicit destroy does | Scripted test + real-Ceph test (volume and root disk move together; cleanup keeps the image) + Postgres integration tests |
| Storage cleanup (`controld/storagegc.rs`, workerd startup cleanup) | Pool sweep by one controld: deletes images of archived/gone sessions and destroyed/gone machines after a grace period, never an open one, audited, dry-run option. Workers at startup drop stale mappings (incl. a fenced host's) and leftover local folders, and never reattach a session whose disk is not mounted. Reports from a worker that no longer owns a session are ignored | Postgres integration tests (fake pool) + real-Ceph test: a crashed host's mapping is released and the disk reopens with its data |
| VM watchdog (`workerd/watchdog.rs`, `controld/crashes.rs`) | Probes every running session and machine VM every 15 s; 3 misses → torn down, reported, restarted by controld (sessions with a continue message); 3 crashes in 30 min stops the restarts | Unit tests (probe, miss counting, machine teardown) + Postgres integration tests (restart, crash loop, machine reboot, stale report ignored) |
| Off-cluster disk backups (`controld/diskbackup.rs`, migration 0035) | Hourly RBD snapshot → full export (first / after 24 diffs) or diff export (skipped if unchanged) → zstd + ChaCha20-Poly1305 frames with a per-backup key → multipart upload → read-back check → `Durable`. Missing shared disk is rebuilt from the chain before use; no backup → the session fails instead of starting empty | Frame tamper test; Postgres + in-process S3 tests (chain, compaction, rebuild, lost-without-backup); real-Ceph test: disk deleted and rebuilt byte-identical |
| Auto-resume after a host dies | A shared-disk session that was mid-turn is queued again at once with a "continue where you left off" message; shared-disk machines are restarted elsewhere | Postgres integration tests |
| Dead host handling (`controld/hostloss.rs`) | Machines with a ready snapshot restored elsewhere at once, others stopped with a reason; running sessions stopped (resumable), booting ones failed, unstarted ones requeued; a returning host is told to kill what the platform stopped | Postgres integration tests, incl. sweep → dead → settled |

**Not done yet, in order:**

1. Desired-state on stop.
2. Auto-resume of a session that was waiting for an answer (today it waits for the answer).
3. Nahim's `puku-proxy` (client reconnect proxy) and `puku-rebuild` (replay installed packages): compiled and unit-tested, not wired. Shared disks keep installed packages, so `puku-rebuild` matters only for host-local disks.
4. Encrypted memory snapshots, so running processes survive and not only files: needs the R0 engine bake-off first (which hypervisor's snapshots are fast enough).
5. Production object storage on our own hardware (MinIO / Ceph RGW): code already speaks S3; this is deployment work.
6. Multi-host chaos runs (two workerd processes on one box first; real hardware for BMC and 3-node Ceph).

How to run the tests: `PUKU_TEST_DATABASE_URL=postgres://… cargo test --workspace` (controld integration tests skip without it); `PUKU_TEST_CEPH=1 cargo test -p puku-volume --test real_ceph` on a host with Ceph and a `client.puku` key.

---

## 5. What's next — start at R0

The phases are gated. **You cannot start R4 (warm resume) without R0's engine numbers.** You cannot claim AC1a (R0 complete) without `bench/R0_REPORT.md`. The plan below is the *recommended* path; you can shuffle R1 ↔ R2 a little, but R0 must close first if you want to ship warm resume in the same release.

### 5.1 Phase R0 — Hypervisor Bake-Off

**Goal.** Pick the primary engine for warm-resume. Until this closes, every org's `quotas.recovery_mode` is forced to `cold_only` (the org ceiling), so even though the per-session default is `warm_allowed`, no session can actually be warm.

**Why it gates warm resume.** Upstream Cloud Hypervisor is unverified for diff memory snapshots (RSD §4.4.2a). Firecracker is verified. libkrun/msb is developer preview. We do not pick a primary engine on an unverified claim.

**What to do (1–2 weeks on a chaos runner).**
1. Stand up four hosts, one per engine: Cloud Hypervisor (current build), libkrun/msb (current SDK), Firecracker (current release), QEMU/KVM (fallback).
2. For each: boot an 8 GB VM from the same base image, take 100 captures at the planned RPO interval, measure pause p50/p95. Restore 100 times, measure p50/p95. Walk a chain of 8 diffs, measure restore.
3. Write `bench/R0_REPORT.md` with the numbers. Record the decision in `crates/puku-workerd/Cargo.toml` (only one engine by default, others feature-gated).
4. Update `preflight.sh` to check the chosen engine.
5. Flip the org ceiling from `cold_only` to `warm_allowed` for the bake-off org. Record in RSD §15.4.

**Output.** `bench/R0_REPORT.md`, primary engine pinned, `preflight.sh` updated, RSD §15.4 decision table filled in.

**Chaos gate.** No chaos test depends on R0. (R0 produces the engine that R4 will exercise.)

### 5.2 Phase R1 — Volume Abstraction

**Goal.** Replace direct host-disk access in `session_actor.rs` with `puku-volume`. Volume still host-pinned after R1 — relocation is R3.

**File-to-file plan.** RSD §13 rows 1, 6, 7, 8, 9, 26, 34. Approximately 1.5–2 weeks.

| Row | File | Why now |
|---|---|---|
| 1 | `migrations/0027_reliability_states.sql` | Foundation. States + `desired_state` + `sla_tier` + `recovery_mode` + `crash_count` + `rpo_at_risk` |
| 6 | `crates/puku-volume/Cargo.toml` + `lib.rs` + `traits.rs` + `types.rs` + `error.rs` | Trait lives before any backend can implement it |
| 7 | `crates/puku-volume/src/local.rs` | Keep host-pinned behavior first; do not migrate RBD on day 1 |
| 8 | `crates/puku-volume/src/rbd.rs` | Real Ceph dev cluster. The `RbdBackend` cookbook is in RSD §4.1.4 |
| 9 | `crates/puku-volume/src/fence.rs` | Blocklist/unblocklist; shares code with `puku-fence` later |
| 26 | `crates/puku-workerd/src/session_actor.rs` (refactor) | Replace `/var/lib/puku/sessions` direct access with `volume.attach()` |
| 34 | `tests/chaos/T1_kill_vmm.rs` | Pass criterion: zero events lost across the kill (no behavior change) |

**Acceptance check.** `cargo test --test kill_vmm` passes. `T1` includes the **fsync'd marker file** assertion now (RSD §6 T1 row).

### 5.3 Phase R2 — Leases

**Goal.** Detect host death in 1–3 s.

**File-to-file plan.** RSD §13 rows 2, 10, 19, 29, 30, 53. Approximately 1 week.

| Row | File | Why now |
|---|---|---|
| 2 | `migrations/0028_leases.sql` | Schema first; sweeper depends on it |
| 10 | `crates/puku-leases/` | `LeaseService` trait, sweeper, heartbeat |
| 19 | `crates/puku-controld/src/leases.rs` | Wraps `LeaseService` with controld config (1 s tick) |
| 29 | `crates/puku-workerd/src/main.rs` (heartbeat task) | Worker renews every 1 s; falls back to read-only on loss |
| 30 | `crates/puku-guestd/src/heartbeat.rs` | Guest → host heartbeat (so guest-hang is detectable) |
| 53 | `tests/chaos/T10_expire_lease.rs` | Worker stops renewing → sweeper marks `suspected` within 3 s |

**Acceptance check.** `T1` still passes (no behavior change). `T10_expire_lease` passes.

### 5.4 Phase R3 — Fencing + Remote Recovery + Session Proxy

**Goal.** Make split-brain impossible. Make failover invisible to the client. This is the phase where things get serious.

**File-to-file plan.** RSD §13 rows 3 (partial: status model only), 4, 11, 18, 20, 21, 22, 23, 25 (partial), 36, 37, 38, 41, 52, 57. Approximately 3–4 weeks. **The longest is `puku-proxy` (row 52, 3 days).**

| Row | File | Why now |
|---|---|---|
| 4 | `migrations/0030_fence_log.sql` | Audit log is read by the runbook; write it from the start |
| 11 | `crates/puku-fence/` | `Fence` trait + Ceph blocklist + IPMI + Redfish + audit |
| 18 | `crates/puku-cloud-proto/src/v2/` | Wire types for lease, fence, volume, snapshot |
| 20 | `crates/puku-controld/src/fence.rs` | Wraps `Fence` with controld logging |
| 21 | `crates/puku-controld/src/recovery.rs` | The `RecoveryChoice::{Local, Remote}` decision; **fence BEFORE attach** (RSD §5.1.1, hard rule) |
| 22 | `crates/puku-controld/src/sweeper.rs` | 1 s lease sweep, 60 s tier transitions, nightly test-restore |
| 23 | `crates/puku-controld/src/scheduler.rs` (extend) | Fingerprint-aware placement |
| 25 | `crates/puku-controld/src/api/mod.rs` (extend) | New endpoints: `POST /v1/admin/fence/{host_id}`, new fields, 409 codes |
| 52 | `crates/puku-proxy/src/main.rs` | `reconnect_token`, replay from `last_seq`, stateless overall, stateful per session |
| 36 | `tests/chaos/T3_crash_loop.rs` | 3 crashes in 10 min → remote eviction |
| 37 | `tests/chaos/T4_host_kernel_panic.rs` | Lease expires + fence + remote restart, with fsync'd marker assertion (RSD §6 T4 row) |
| 38 | `tests/chaos/T5_network_partition.rs` | Blocklist before any relocate; old-host write fails |
| 41 | `tests/chaos/T8_kill_controld.rs` | Other controld instances pick up via NOTIFY; no 5xx spike |
| 57 | `tests/chaos/T14_proxy_failover.rs` | WS survives controld kill mid-`waiting_input` |

**Acceptance check.** `T3`, `T4`, `T5`, `T8`, `T14` all pass. **Manual**: inspect `fence_log` — every cross-host relocate has a preceding `blocklist` row. That manual check is what proves F6 (split-brain impossible).

### 5.5 Phase R4 — Warm Resume + Post-Restore Refresh

**Goal.** Co-issued disk+mem snapshot, incremental diff chain, background merge, idle trigger, RPO constant, test-restore. Only after R0 picks an engine that supports diff memory snapshots.

**File-to-file plan.** RSD §13 rows 3 (full), 5, 12, 13, 14, 15, 15a, 16, 17, 27, 28, 30, 31, 32, 33, 35, 39, 40, 50. Approximately 4–5 weeks. **Longest phase** because the snapshot stack is new.

| Row | File | Why now |
|---|---|---|
| 3 | `migrations/0029_snapshots.sql` | Now you can write the table — engine chosen |
| 5 | `migrations/0031_packages.sql` | Needed by `puku-guestd` package watcher below |
| 12 | `crates/puku-snapshot/Cargo.toml` + `lib.rs` + `manifest.rs` | Library skeleton; `Manifest` + `SnapshotStatus` |
| 13 | `crates/puku-snapshot/src/capture.rs` | The 3-phase sequence (RSD §4.4.2) — Freeze, Persist, Durable |
| 14 | `crates/puku-snapshot/src/restore.rs` | Restore ladder, walks the parent chain |
| 15 | `crates/puku-snapshot/src/retention.rs` | Newest-first reaper after compaction |
| 15a | `crates/puku-snapshot/src/compaction.rs` | Overlay merge, B' cut-over, re-parenting in one transaction |
| 16 | `crates/puku-snapshot/src/test_restore.rs` | Nightly background verifier |
| 17 | `crates/puku-snapshot/src/idle_signal.rs` | vsock listener for "between tool calls" |
| 17b | `crates/puku-snapshot/src/triggers.rs` | `effective_recovery_mode()` + `rpo_seconds(tier)` — the only place that decides |
| 17c | `crates/puku-snapshot/src/chain.rs` | Diff-chain bookkeeping (current base, next-to-compact) |
| 17d | `crates/puku-snapshot/src/disk_backup.rs` | Periodic off-cluster backup (RSD §4.7) — can also ship in R5 |
| 27 | `crates/puku-workerd/src/session_actor.rs` (idle snapshot) | Listens for idle signal, calls `capture_paired()` |
| 28 | `crates/puku-workerd/src/session_actor.rs` (desired_state) | Distinguishes expected shutdown from crash |
| 31 | `crates/puku-guestd/src/idle.rs` | Watches puku-cli control channel, sends `idle_for_snapshot` |
| 32 | `crates/puku-guestd/src/shutdown.rs` (extend) | `clean_shutdown` message on SIGTERM |
| 33 | `crates/puku-observability/src/scrub.rs` (extend) | New Sentry tags + metrics |
| 50 | `crates/puku-guestd/src/restore_refresh.rs` | Clock, machine-id, RNG reset on every restore |
| 35 | `tests/chaos/T2_guest_hang.rs` | vsock heartbeat stops → kill + restart |
| 39 | `tests/chaos/T6_corrupt_snapshot.rs` | SHA-256 mismatch → walk to previous Durable |
| 40 | `tests/chaos/T7_warm_resume_after_kill.rs` | SIGKILL during tool call → warm resume, tool re-runs from transcript |

**Acceptance check.** `T2`, `T6`, `T7` all pass. **Property test**: 100 captures → `chain depth ≤ MERGE_DEPTH + SNAPSHOT_BUFFER_MAX` at every point.

### 5.6 Phase R5 — Tiered Retention + Environment Rebuild + Disk Backup

**Goal.** `hibernated`, `cold_archived`, `archived` states. EC pool for cold. Per-org tier config. `puku-rebuild`. `disk_backup.rs` (if not already in R4).

**File-to-file plan.** RSD §13 rows 24, 25 (complete), 42, 49, 51, 54, 55, 58, 59, 60, 61, 62. Approximately 2–3 weeks.

| Row | File | Why now |
|---|---|---|
| 24 | `crates/puku-controld/src/archive.rs` (extend) | Tier transition table (RSD §7) |
| 25 | `crates/puku-controld/src/api/mod.rs` (complete) | `/resume` accepts `archived` and long-polls |
| 42 | `tests/chaos/T9_no_silent_delete.rs` | 30-day mocked → `cold_archived` not `archived`; R2 archive sha256 present |
| 49 | `crates/puku-guestd/src/package_watcher.rs` | inotify on `/var/lib/dpkg/status`, pip dist-info, npm cache |
| 51 | `crates/puku-rebuild/src/main.rs` | Emits shell script from `installed_packages` |
| 54 | `tests/chaos/T11_recover_from_hibernated.rs` | `hibernated` → `running` within 30 s, RBD attached, mem snap restored |
| 55 | `tests/chaos/T12_recover_from_archived.rs` | `archived` → download R2 archive, sha256 verify, fresh clone, apply manifest |
| 58 | `tests/chaos/T15_cold_resume.rs` | `cold_only` + `apt install jq` + SIGKILL → package still there, no rollback |
| 59 | `tests/chaos/T16_rebuild_from_packages.rs` | Fresh base + replayed installs = same env |
| 60 | `migrations/0032_disk_backups.sql` | `disk_backups` table + `last_disk_backup_at` + `disk_backup_interval_s` |
| 61 | `crates/puku-snapshot/src/disk_backup.rs` (if not in R4) | `rbd export`/`export-diff` to R2, 24-diff compaction |
| 62 | `tests/chaos/T17_ceph_pool_loss_restore.rs` | Delete RBD image → `FromArchive`; files + packages present |

**Acceptance check.** All five chaos tests pass. `AC19` verified (off-cluster disk backup restores after RBD loss).

### 5.7 Phase R6 — CPU Parity + Preflight

**Goal.** Workers publish fingerprint; dispatcher enforces match; preflight extends.

**File-to-file plan.** RSD §13 rows 43, 44, 45, 46, 47, 48, 56. Approximately 1 week.

| Row | File | Why now |
|---|---|---|
| 43 | `deploy/scripts/prestage-rbd.sh` | Run once per cluster; includes `puku-snap-hot` pool |
| 44 | `deploy/scripts/preflight.sh` (extend) | Hot pool size=3, write/read/delete test, cache-mode check, fingerprint check |
| 45 | `deploy/systemd/puku-workerd.service` (extend) | `PUKU_VOLUME_BACKEND=rbd` |
| 46 | `deploy/scripts/upgrade-cluster.sh` | Rolling upgrade with drain bounded by 120 s |
| 47 | `.github/workflows/chaos.yml` | PR gate; serial because tests kill hosts |
| 48 | `docs/RELIABILITY-RUNBOOK.md` | Operator workflow |
| 56 | `tests/chaos/T13_snapshot_migrated_to_incompatible_host.rs` | Snapshot needs `avx512`, host lacks it → clear error, no half-restore |

**Acceptance check.** `T13` passes with the expected error message. PR fails if a chaos test fails.

---

## 6. File-by-file progress against RSD §13

The table below mirrors RSD §13 but adds a **Status** column. The first column is the row number in RSD §13. "Spec" = the design is in the RSD; you can build from it. "Code" = the file is on disk and `cargo build` / `cargo test` succeeds against it.

| Row | File | Spec | Code | Notes |
|---|---|---|---|---|
| 1 | `migrations/0027_reliability_states.sql` | ✅ | ⬜ | Foundation |
| 2 | `migrations/0028_leases.sql` | ✅ | ⬜ | |
| 3 | `migrations/0029_snapshots.sql` | ✅ | ⬜ | 3-state status + parent FK RESTRICT |
| 4 | `migrations/0030_fence_log.sql` | ✅ | ⬜ | |
| 5 | `migrations/0031_packages.sql` | ✅ | ⬜ | |
| 6 | `crates/puku-volume/Cargo.toml` + `lib.rs` + `traits.rs` + `types.rs` + `error.rs` | ✅ | ⬜ | |
| 7 | `crates/puku-volume/src/local.rs` | ✅ | ⬜ | |
| 8 | `crates/puku-volume/src/rbd.rs` | ✅ | ⬜ | |
| 9 | `crates/puku-volume/src/fence.rs` | ✅ | ⬜ | |
| 10 | `crates/puku-leases/Cargo.toml` + `lib.rs` + `heartbeat.rs` + `sweeper.rs` | ✅ | ⬜ | |
| 11 | `crates/puku-fence/Cargo.toml` + `lib.rs` + `ceph.rs` + `ipmi.rs` + `redfish.rs` + `audit.rs` | ✅ | ⬜ | |
| 12 | `crates/puku-snapshot/Cargo.toml` + `lib.rs` + `manifest.rs` | ✅ | ⬜ | |
| 13 | `crates/puku-snapshot/src/capture.rs` | ✅ | ⬜ | 3-phase sequence |
| 14 | `crates/puku-snapshot/src/restore.rs` | ✅ | ⬜ | |
| 15 | `crates/puku-snapshot/src/retention.rs` | ✅ | ⬜ | |
| 15a | `crates/puku-snapshot/src/compaction.rs` | ✅ | ⬜ | New row (P8) |
| 16 | `crates/puku-snapshot/src/test_restore.rs` | ✅ | ⬜ | |
| 17 | `crates/puku-snapshot/src/idle_signal.rs` | ✅ | ⬜ | |
| 18 | `crates/puku-cloud-proto/src/v2/lease.rs` + `fence.rs` + `volume.rs` + `snapshot.rs` | ✅ | ⬜ | |
| 19 | `crates/puku-controld/src/leases.rs` | ✅ | ⬜ | |
| 20 | `crates/puku-controld/src/fence.rs` | ✅ | ⬜ | |
| 21 | `crates/puku-controld/src/recovery.rs` | ✅ | ⬜ | Fence-before-attach hard rule |
| 22 | `crates/puku-controld/src/sweeper.rs` | ✅ | ⬜ | |
| 23 | `crates/puku-controld/src/scheduler.rs` (extend) | ✅ | ⬜ | |
| 24 | `crates/puku-controld/src/archive.rs` (extend) | ✅ | ⬜ | |
| 25 | `crates/puku-controld/src/api/mod.rs` (extend) | ✅ | ⬜ | 409 codes, not silent downgrade |
| 26 | `crates/puku-workerd/src/session_actor.rs` (refactor) | ✅ | ⬜ | |
| 27 | `crates/puku-workerd/src/session_actor.rs` (idle snapshot) | ✅ | ⬜ | |
| 28 | `crates/puku-workerd/src/session_actor.rs` (desired_state check) | ✅ | ⬜ | |
| 29 | `crates/puku-workerd/src/main.rs` (lease heartbeat) | ✅ | ⬜ | |
| 30 | `crates/puku-guestd/src/heartbeat.rs` | ✅ | ⬜ | |
| 31 | `crates/puku-guestd/src/idle.rs` | ✅ | ⬜ | |
| 32 | `crates/puku-guestd/src/shutdown.rs` (extend) | ✅ | ⬜ | |
| 33 | `crates/puku-observability/src/scrub.rs` (extend) | ✅ | ⬜ | |
| 34 | `tests/chaos/T1_kill_vmm.rs` | ✅ | ⬜ | Fsync'd marker assertion |
| 35 | `tests/chaos/T2_guest_hang.rs` | ✅ | ⬜ | |
| 36 | `tests/chaos/T3_crash_loop.rs` | ✅ | ⬜ | |
| 37 | `tests/chaos/T4_host_kernel_panic.rs` | ✅ | ⬜ | Fsync'd marker assertion (cold_only) |
| 38 | `tests/chaos/T5_network_partition.rs` | ✅ | ⬜ | |
| 39 | `tests/chaos/T6_corrupt_snapshot.rs` | ✅ | ⬜ | |
| 40 | `tests/chaos/T7_warm_resume.rs` | ✅ | ⬜ | Tool re-runs from transcript |
| 41 | `tests/chaos/T8_kill_controld.rs` | ✅ | ⬜ | |
| 42 | `tests/chaos/T9_no_silent_delete.rs` | ✅ | ⬜ | |
| 43 | `deploy/scripts/prestage-rbd.sh` | ✅ | ⬜ | Includes `puku-snap-hot` |
| 44 | `deploy/scripts/preflight.sh` (extend) | ✅ | ⬜ | Hot pool + cache-mode checks |
| 45 | `deploy/systemd/puku-workerd.service` (extend) | ✅ | ⬜ | |
| 46 | `deploy/scripts/upgrade-cluster.sh` | ✅ | ⬜ | Drain bounded by 120 s |
| 47 | `.github/workflows/chaos.yml` | ✅ | ⬜ | |
| 48 | `docs/RELIABILITY-RUNBOOK.md` | ✅ | ⬜ | |
| 49 | `crates/puku-guestd/src/package_watcher.rs` | ✅ | ⬜ | |
| 50 | `crates/puku-guestd/src/restore_refresh.rs` | ✅ | ⬜ | |
| 51 | `crates/puku-rebuild/src/main.rs` | ✅ | ⬜ | |
| 52 | `crates/puku-proxy/src/main.rs` | ✅ | ⬜ | 3 days (longest single file) |
| 53 | `tests/chaos/T10_expire_lease.rs` | ✅ | ⬜ | |
| 54 | `tests/chaos/T11_recover_from_hibernated.rs` | ✅ | ⬜ | |
| 55 | `tests/chaos/T12_recover_from_archived.rs` | ✅ | ⬜ | |
| 56 | `tests/chaos/T13_snapshot_migrated_to_incompatible_host.rs` | ✅ | ⬜ | |
| 57 | `tests/chaos/T14_proxy_failover.rs` | ✅ | ⬜ | |
| 58 | `tests/chaos/T15_cold_resume.rs` | ✅ | ⬜ | |
| 59 | `tests/chaos/T16_rebuild_from_packages.rs` | ✅ | ⬜ | |
| 60 | `migrations/0032_disk_backups.sql` | ✅ | ⬜ | |
| 61 | `crates/puku-snapshot/src/disk_backup.rs` | ✅ | ⬜ | |
| 62 | `tests/chaos/T17_ceph_pool_loss_restore.rs` | ✅ | ⬜ | |

**Summary.** Stale: written before the code. See §4 for what is built and tested on `mahi`.

---

## 7. Honest gaps (RSD §12, copied)

These cannot be solved by software alone. The rebuild does not promise to fix them.

1. **True zero-loss memory recovery** is not realistic without Remus/COLO (QEMU/Xen only). We promise 5 s / 60 s RPO for premium / standard warm resume; cold_only sessions lose RAM entirely.
2. **Ceph needs operational skill** — 3 storage nodes, 25 GbE, monitor quorum. Mitigation: `prestage-rbd.sh` automates initial setup; `preflight.sh` validates.
3. **Snapshot features depend on hypervisor version** — libkrun/CH/Firecracker diff snapshot support varies. Verify on the version we ship in CI on every PR.
4. **Live migration** is only worth it for > 50-host fleets. Out of scope for reliability rebuild.
5. **Backups are periodic.** A whole-pool loss costs up to `disk_backup_interval` of disk writes; shorten the interval per tier if that is not acceptable.

---

## 8. Decisions deferred to R0 (RSD §15.4)

| Question | Default until R0 closes | Decision lives in |
|---|---|---|
| Primary engine | (none) — every org's ceiling is `cold_only` | `bench/R0_REPORT.md`, RSD §15.4 decision table |
| Per-org warm ceiling | forced to `cold_only` | operator flips after R0 picks an engine |
| Live migration | out of scope (R7) | added later if fleet > 50 hosts |

---

## 9. How to use this file

1. **You are starting the rebuild.** Read `docs/RELIABILITY-REBUILD.md` §0–§3 in full. Then start Phase R0.
2. **You are mid-phase.** Open RSD §13, find your row, read the matching spec section, build, run the chaos test listed in the row's "Done when" column.
3. **You hit a contradiction.** Check whether the RSD was updated. If it disagrees with this PLAN.md, the RSD wins — this file is a tracker, not a spec. If the RSD is silent, the convention is: monotonic mode (sessions can only lower `recovery_mode`), fence-before-attach (always), newest-first delete (always), sha256-verify-then-apply (always).
4. **You are doing code review.** Open RSD §10 (acceptance criteria) and RSD §6 (chaos tests). Every check is on a row.

**When in doubt, open the RSD.** It is the spec.
