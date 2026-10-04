//! Guest images for Cloud Hypervisor.
//!
//! msb pulls OCI images into a store of its own. Cloud Hypervisor boots a
//! disk, so an OCI image is turned into one ahead of time by
//! `deploy/scripts/build-ch-rootfs.sh`, which writes:
//!
//! ```text
//! <images_dir>/<key>/rootfs.ext4         the image's filesystem + /sbin/puku-guestd
//! <images_dir>/<key>/image-config.json   its ENV/WORKDIR/USER, also inside the disk
//! <images_dir>/<key>/image.json          which image it is: ref, id, registry digests
//! ```
//!
//! `<key>` is the image reference made filesystem-safe, so the same string
//! controld sends (`PUKU_AGENT_IMAGE`, a machine's `image`) finds the disk.
//! The staged list goes to controld in every heartbeat, so a machine is only
//! placed where its disk is.

use std::path::{Path, PathBuf};

use anyhow::Result;
use puku_cloud_proto::worker_proto::StagedImage;
use puku_cloud_proto::Engine;
use serde::Deserialize;

use crate::vm::BootRefusal;

/// `docker.io/poridhi/puku-agent:0.1.0` -> `docker.io_poridhi_puku-agent_0.1.0`.
/// The rule lives in the protocol crate: controld checks for the same key
/// before it places a machine here.
pub fn key(image: &str) -> String {
    puku_cloud_proto::machine::image_key(image)
}

pub fn rootfs(images_dir: &Path, image: &str) -> Result<PathBuf> {
    let disk = images_dir.join(key(image)).join("rootfs.ext4");
    if !disk.is_file() {
        return Err(BootRefusal::new(
            "image_not_staged",
            format!(
                "image {image} is not staged for cloud_hypervisor (no {}); run \
                 deploy/scripts/build-ch-rootfs.sh {image}",
                disk.display()
            ),
        )
        .into());
    }
    Ok(disk)
}

/// What build-ch-rootfs.sh records about the image it staged.
#[derive(Deserialize, Default)]
struct ImageInfo {
    #[serde(rename = "ref")]
    reference: Option<String>,
    id: Option<String>,
    #[serde(default)]
    repo_digests: Vec<String>,
}

impl ImageInfo {
    /// The registry digest when the image came from one, else the local
    /// image id -- both `sha256:` of the same bits.
    fn digest(&self) -> Option<String> {
        self.repo_digests
            .iter()
            .find_map(|d| d.split_once('@').map(|(_, digest)| digest.to_string()))
            .or_else(|| self.id.clone())
    }
}

/// Every image staged under `images_dir`. A disk staged before `image.json`
/// existed is listed without a digest.
pub fn staged(images_dir: &Path) -> Vec<StagedImage> {
    let Ok(entries) = std::fs::read_dir(images_dir) else { return Vec::new() };
    let mut out: Vec<StagedImage> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let key = e.file_name().to_str()?.to_string();
            // build-ch-rootfs.sh works in `.build-*` and swaps in atomically.
            if key.starts_with('.') {
                return None;
            }
            let disk = std::fs::metadata(e.path().join("rootfs.ext4")).ok().filter(|m| m.is_file())?;
            let info: ImageInfo = std::fs::read(e.path().join("image.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            Some(StagedImage {
                engine: Engine::CloudHypervisor,
                digest: info.digest(),
                image: info.reference,
                key,
                size_mib: Some(disk.len() / (1024 * 1024)),
            })
        })
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_the_protocol_crates() {
        assert_eq!(key("docker.io/poridhi/puku-agent:0.1.0"), "docker.io_poridhi_puku-agent_0.1.0");
        assert_eq!(key("img@sha256:abc"), "img_sha256_abc");
    }

    #[test]
    fn an_unstaged_image_says_how_to_stage_it_and_why() {
        let err = rootfs(Path::new("/nonexistent"), "alpine").unwrap_err();
        assert!(err.to_string().contains("build-ch-rootfs.sh alpine"), "{err}");
        assert_eq!(crate::vm::boot_failure_reason(&err), "image_not_staged");
    }

    #[test]
    fn staged_images_are_listed_with_their_digest() {
        let dir = std::env::temp_dir().join(format!("puku-images-{}", uuid::Uuid::new_v4()));
        let with = dir.join(key("pukubot:latest"));
        std::fs::create_dir_all(&with).unwrap();
        std::fs::write(with.join("rootfs.ext4"), b"disk").unwrap();
        std::fs::write(
            with.join("image.json"),
            br#"{"ref":"pukubot:latest","id":"sha256:local","repo_digests":["reg/pukubot@sha256:remote"]}"#,
        )
        .unwrap();
        let old = dir.join(key("old:1"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("rootfs.ext4"), b"disk").unwrap();
        std::fs::create_dir_all(dir.join(".build-abc")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();

        let got = staged(&dir);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].key, "old_1");
        assert!(got[0].digest.is_none(), "staged before image.json existed");
        assert_eq!(got[1].image.as_deref(), Some("pukubot:latest"));
        assert_eq!(got[1].digest.as_deref(), Some("sha256:remote"), "the registry digest wins");
        assert_eq!(got[1].engine, Engine::CloudHypervisor);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
