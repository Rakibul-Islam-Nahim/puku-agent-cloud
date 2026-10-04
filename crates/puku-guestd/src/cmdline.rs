//! The kernel command line keys puku-guestd reads. Nothing secret goes here:
//! /proc/cmdline is world-readable inside the guest. Credentials arrive over
//! vsock in `GuestRequest::Init` instead.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// A virtiofs share: the tag Cloud Hypervisor exports it under, and where it
/// is mounted in the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub tag: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boot {
    /// Block device holding the writable upper layer of the root overlay.
    pub overlay: Option<String>,
    pub shares: Vec<Share>,
    /// `a.b.c.d/prefix`
    pub ip: Option<String>,
    pub gateway: Option<String>,
    pub dns: Option<String>,
    pub hostname: Option<String>,
    /// Size of /dev/shm; Chromium needs far more than the 64 MiB default.
    pub shm: Option<String>,
}

pub fn parse(cmdline: &str) -> Boot {
    let mut b = Boot::default();
    for word in cmdline.split_whitespace() {
        let Some((k, v)) = word.split_once('=') else { continue };
        match k {
            "puku.overlay" => b.overlay = Some(v.to_string()),
            "puku.ip" => b.ip = Some(v.to_string()),
            "puku.gw" => b.gateway = Some(v.to_string()),
            "puku.dns" => b.dns = Some(v.to_string()),
            "puku.hostname" => b.hostname = Some(v.to_string()),
            "puku.shm" => b.shm = Some(v.to_string()),
            "puku.fs" => {
                for share in v.split(',').filter(|s| !s.is_empty()) {
                    if let Some((tag, path)) = share.split_once(':') {
                        if path.starts_with('/') && !tag.is_empty() {
                            b.shares.push(Share { tag: tag.to_string(), path: path.to_string() });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    b
}

/// `10.200.0.6/30` -> (address, netmask), both as dotted quads' octets.
pub fn parse_cidr(cidr: &str) -> Option<([u8; 4], [u8; 4])> {
    let (addr, prefix) = cidr.split_once('/')?;
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    let prefix: u32 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some((ip.octets(), mask.to_be_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_the_worker_writes() {
        let b = parse(
            "console=hvc0 root=/dev/vda ro init=/sbin/puku-guestd puku.overlay=/dev/vdb \
             puku.fs=fs0:/session,fs1:/workspace puku.ip=10.200.0.6/30 puku.gw=10.200.0.5 \
             puku.dns=10.200.0.5 puku.hostname=ses-abc puku.shm=512m quiet",
        );
        assert_eq!(b.overlay.as_deref(), Some("/dev/vdb"));
        assert_eq!(
            b.shares,
            vec![
                Share { tag: "fs0".into(), path: "/session".into() },
                Share { tag: "fs1".into(), path: "/workspace".into() },
            ]
        );
        assert_eq!(b.ip.as_deref(), Some("10.200.0.6/30"));
        assert_eq!(b.gateway.as_deref(), Some("10.200.0.5"));
        assert_eq!(b.hostname.as_deref(), Some("ses-abc"));
        assert_eq!(b.shm.as_deref(), Some("512m"));
    }

    #[test]
    fn a_relative_mount_path_is_ignored() {
        assert!(parse("puku.fs=fs0:relative").shares.is_empty());
    }

    #[test]
    fn cidrs_become_address_and_mask() {
        assert_eq!(parse_cidr("10.200.0.6/30"), Some(([10, 200, 0, 6], [255, 255, 255, 252])));
        assert_eq!(parse_cidr("0.0.0.0/0"), Some(([0, 0, 0, 0], [0, 0, 0, 0])));
        assert_eq!(parse_cidr("10.0.0.1"), None);
        assert_eq!(parse_cidr("10.0.0.1/33"), None);
    }
}
