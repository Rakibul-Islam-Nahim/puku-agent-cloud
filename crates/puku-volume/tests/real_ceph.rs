//! Runs `RbdBackend` against a real Ceph cluster. Skipped unless
//! `PUKU_TEST_CEPH=1`; needs pools `puku-base` / `puku-sessions`, a
//! protected `puku-base/agent-base@v1` (ext4), cephx user `puku`, root.
//!
//!   PUKU_TEST_CEPH=1 cargo test -p puku-volume --test real_ceph -- --nocapture
//!
//! Two backends with `noshare` mappings act as two hosts on one machine.

use std::process::Command;

use puku_volume::{HostId, RbdBackend, RbdBackendConfig, SnapId, VolumeBackend, VolumeId};
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("PUKU_TEST_CEPH").as_deref() == Ok("1")
}

fn backend() -> RbdBackend {
    RbdBackend::new(RbdBackendConfig::new("puku-base", "puku-sessions").with_map_options(&["noshare"]))
}

fn sh(cmd: &str) -> (i32, String) {
    let out = Command::new("sh").arg("-c").arg(cmd).output().expect("sh");
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[tokio::test]
async fn fence_cuts_off_the_old_writer_and_the_new_host_takes_over() {
    if !enabled() {
        eprintln!("PUKU_TEST_CEPH not set; skipping");
        return;
    }
    let (host_a, host_b) = (HostId(Uuid::new_v4()), HostId(Uuid::new_v4()));
    let (a, b) = (backend(), backend());
    let session = Uuid::new_v4();

    // Create from the configured base and map on host A.
    let vol: VolumeId = a.create(session, &SnapId::new(VolumeId(String::new()), ""), host_a).await.expect("clone");
    let dev_a = a.attach(&vol, host_a).await.expect("map on A");
    let mnt_a = format!("/mnt/pukutest-a-{session}");
    assert_eq!(sh(&format!("mkdir -p {mnt_a} && mount {} {mnt_a} && echo from-A > {mnt_a}/marker && sync", dev_a.as_path().display())).0, 0);

    // Host B cannot take it while A holds the exclusive lock. (A raw map:
    // on one machine B's backend would otherwise reuse A's local mapping,
    // which on two real hosts it cannot see.)
    let (rc, out) = sh(&format!("rbd device map --exclusive -o noshare {vol}"));
    assert_ne!(rc, 0, "B must be refused while A holds the lock: {out}");

    // Fence: every client with the volume open is blocklisted, verified.
    let fenced = b.fence_volume(&vol).await.expect("fence");
    assert_eq!(fenced.len(), 1, "exactly A's client: {fenced:?}");
    let listed = b.blocklisted().await.expect("blocklist ls");
    assert!(listed.contains(&fenced[0]));

    // A can no longer write.
    let (rc, out) = sh(&format!("dd if=/dev/zero of={} bs=4k count=1 oflag=direct seek=1000 status=none", dev_a.as_path().display()));
    assert_ne!(rc, 0, "A's direct write must fail after the fence: {out}");

    // A is dead from here on: drop its mapping from this machine so B's
    // view matches a separate host's (it never sees A's devices).
    sh(&format!("umount -l {mnt_a}; rbd device unmap -o force {}", dev_a.as_path().display()));

    // B maps (retry: the lock moves once A's client is gone) and sees A's data.
    let mut dev_b = None;
    for _ in 0..6 {
        match b.attach(&vol, host_b).await {
            Ok(d) => {
                dev_b = Some(d);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
        }
    }
    let dev_b = dev_b.expect("B maps after the fence");
    let mnt_b = format!("/mnt/pukutest-b-{session}");
    let (rc, out) = sh(&format!("mkdir -p {mnt_b} && mount {} {mnt_b} && cat {mnt_b}/marker; umount {mnt_b}", dev_b.as_path().display()));
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("from-A"), "B must see A's fsynced data: {out}");

    // Cleanup: unmap both, lift the fence, delete the volume.
    b.detach(&vol, host_b).await.expect("unmap B");
    b.unfence_addrs(&fenced).await.expect("unfence");
    sh(&format!("rbd rm {vol} >/dev/null 2>&1"));
}
