//! Per-VM networking for Cloud Hypervisor: a TAP device on a /30 of its own,
//! NAT out, and an nftables policy.
//!
//! msb does all of this inside its own process with a userspace TCP/IP
//! stack. Cloud Hypervisor hands the guest a real NIC, so the host has to do
//! it: the guest gets `10.200.<n>`, the host end of its TAP is its gateway
//! and resolver, and the `puku` nftables table decides what it may reach:
//!
//! * never the host (except DNS on its own gateway), never another VM, never
//!   a private or link-local range (cloud metadata lives there);
//! * in allowlist mode, only addresses the DNS filter (`dns.rs`) has
//!   resolved for an allowed domain, each expiring with its TTL.

use std::net::Ipv4Addr;
use std::process::Stdio;

use anyhow::{bail, Context, Result};

/// VMs are carved out of `10.200.0.0/16` in /30s: host, guest, and the two
/// the subnet reserves. Slot 0 is never used, so no address ends in `.0.1`
/// ambiguity with a pool's first address.
pub const POOL_BASE: [u8; 4] = [10, 200, 0, 0];
pub const MAX_SLOTS: u32 = 16_383;

/// Everything derived from a VM's slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSlot {
    pub slot: u32,
    pub tap: String,
    pub host: Ipv4Addr,
    pub guest: Ipv4Addr,
    pub mac: String,
    /// vsock context id: 3 is the first a guest may have.
    pub cid: u32,
}

impl NetSlot {
    pub fn new(slot: u32) -> NetSlot {
        assert!((1..=MAX_SLOTS).contains(&slot), "slot {slot} out of range");
        let base = u32::from_be_bytes(POOL_BASE) + slot * 4;
        let [_, _, b, c] = slot.to_be_bytes();
        NetSlot {
            slot,
            tap: format!("pkt{slot}"),
            host: Ipv4Addr::from(base + 1),
            guest: Ipv4Addr::from(base + 2),
            mac: format!("52:54:00:70:{b:02x}:{c:02x}"),
            cid: slot + 2,
        }
    }

    pub fn allow_set(&self) -> String {
        format!("allow_{}", self.slot)
    }

    pub fn chain(&self) -> String {
        format!("vm_{}", self.slot)
    }
}

/// The table every VM shares. Idempotent: `add` does nothing to what exists.
pub fn base_ruleset() -> String {
    // Priority -10 runs ahead of anything Docker installs at 0. An accept
    // here does not stop Docker's own DROP policy from dropping the packet
    // later, which is why `ensure_base` also opens DOCKER-USER for pkt*.
    "add table inet puku
add set inet puku blocked4 { type ipv4_addr; flags interval; elements = { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 240.0.0.0/4 } }
add map inet puku vm_policy { type ifname : verdict; }
add chain inet puku forward { type filter hook forward priority -10; policy accept; }
add chain inet puku from_vm
add chain inet puku input { type filter hook input priority -10; policy accept; }
add chain inet puku postrouting { type nat hook postrouting priority 100; policy accept; }
flush chain inet puku forward
flush chain inet puku from_vm
flush chain inet puku input
flush chain inet puku postrouting
add rule inet puku forward iifname \"pkt*\" jump from_vm
add rule inet puku from_vm meta nfproto ipv6 drop
add rule inet puku from_vm ip daddr @blocked4 drop
add rule inet puku from_vm iifname vmap @vm_policy
add rule inet puku input iifname \"pkt*\" udp dport 53 accept
add rule inet puku input iifname \"pkt*\" tcp dport 53 accept
add rule inet puku input iifname \"pkt*\" drop
add rule inet puku postrouting ip saddr 10.200.0.0/16 oifname != \"pkt*\" masquerade
"
    .to_string()
}

/// Per-VM policy in allowlist mode: its own IP set, filled by the DNS
/// filter, and a chain that admits only that set.
pub fn allowlist_ruleset(n: &NetSlot) -> String {
    let (set, chain, tap) = (n.allow_set(), n.chain(), &n.tap);
    format!(
        "add set inet puku {set} {{ type ipv4_addr; flags timeout; }}
add chain inet puku {chain}
flush chain inet puku {chain}
add rule inet puku {chain} ip daddr @{set} accept
add rule inet puku {chain} drop
add element inet puku vm_policy {{ \"{tap}\" : jump {chain} }}
"
    )
}

/// Undo `allowlist_ruleset`. Each line on its own, so one already missing
/// does not stop the rest.
pub fn allowlist_teardown(n: &NetSlot) -> Vec<String> {
    vec![
        format!("delete element inet puku vm_policy {{ \"{}\" }}", n.tap),
        format!("delete chain inet puku {}", n.chain()),
        format!("delete set inet puku {}", n.allow_set()),
    ]
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn nft(script: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running nft")?;
    child.stdin.take().unwrap().write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("nft: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Once per worker start: forwarding on, the shared table, and a hole in
/// Docker's FORWARD chain for our TAPs. The last matters on the production
/// box, which runs Docker: its FORWARD policy is DROP, and a packet has to
/// be accepted by every base chain on the hook, not just ours.
pub fn ensure_base() -> Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").context("enabling IPv4 forwarding")?;
    nft(&base_ruleset())?;
    if run("iptables", &["-nL", "DOCKER-USER"]).is_ok() {
        for rule in [
            &["-i", "pkt+", "-j", "ACCEPT"][..],
            &["-o", "pkt+", "-m", "conntrack", "--ctstate", "RELATED,ESTABLISHED", "-j", "ACCEPT"][..],
        ] {
            let mut check = vec!["-C", "DOCKER-USER"];
            check.extend_from_slice(rule);
            if run("iptables", &check).is_err() {
                let mut insert = vec!["-I", "DOCKER-USER"];
                insert.extend_from_slice(rule);
                run("iptables", &insert)?;
            }
        }
    }
    Ok(())
}

/// Create a VM's TAP and, in allowlist mode, its policy.
pub fn up(n: &NetSlot, allowlist: bool) -> Result<()> {
    // A TAP left behind by a VM that died with the box would make `add` fail.
    let _ = run("ip", &["link", "del", &n.tap]);
    run("ip", &["tuntap", "add", "dev", &n.tap, "mode", "tap"])?;
    run("ip", &["addr", "add", &format!("{}/30", n.host), "dev", &n.tap])?;
    run("ip", &["link", "set", &n.tap, "up"])?;
    if allowlist {
        nft(&allowlist_ruleset(n))?;
    }
    Ok(())
}

/// Remove a VM's TAP and policy. Best effort throughout: this runs on
/// teardown paths that must not stop half way.
pub fn down(n: &NetSlot) {
    for line in allowlist_teardown(n) {
        let _ = nft(&line);
    }
    let _ = run("ip", &["link", "del", &n.tap]);
}

/// Admit `ip` for this VM until `ttl_s` from now.
pub fn allow(n: &NetSlot, ip: Ipv4Addr, ttl_s: u32) -> Result<()> {
    nft(&format!(
        "add element inet puku {} {{ {ip} timeout {}s }}",
        n.allow_set(),
        ttl_s.clamp(60, 86_400)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_map_to_disjoint_thirty_twos() {
        let a = NetSlot::new(1);
        assert_eq!((a.host, a.guest), (Ipv4Addr::new(10, 200, 0, 5), Ipv4Addr::new(10, 200, 0, 6)));
        assert_eq!(a.tap, "pkt1");
        assert_eq!(a.cid, 3, "3 is the first guest cid");
        let b = NetSlot::new(64);
        assert_eq!((b.host, b.guest), (Ipv4Addr::new(10, 200, 1, 1), Ipv4Addr::new(10, 200, 1, 2)));
        let last = NetSlot::new(MAX_SLOTS);
        assert_eq!(last.guest, Ipv4Addr::new(10, 200, 255, 254));
        assert!(last.tap.len() <= 15, "interface names are at most 15 bytes");
    }

    #[test]
    fn macs_are_unique_and_locally_administered() {
        assert_ne!(NetSlot::new(1).mac, NetSlot::new(2).mac);
        assert!(NetSlot::new(300).mac.starts_with("52:54:00:70:01:2c"));
    }

    /// Everything a VM must never reach is in the drop set, and the only
    /// host service it can reach is DNS.
    #[test]
    fn the_base_table_fences_vms_in() {
        let r = base_ruleset();
        for net in ["10.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12", "192.168.0.0/16", "127.0.0.0/8"] {
            assert!(r.contains(net), "missing {net}");
        }
        assert!(r.contains("iifname \"pkt*\" udp dport 53 accept"));
        assert!(r.contains("iifname \"pkt*\" drop"));
        assert!(r.contains("masquerade"));
        assert!(r.contains("priority -10"));
    }

    #[test]
    fn an_allowlisted_vm_admits_only_its_set() {
        let r = allowlist_ruleset(&NetSlot::new(7));
        assert!(r.contains("add set inet puku allow_7 { type ipv4_addr; flags timeout; }"));
        assert!(r.contains("add rule inet puku vm_7 ip daddr @allow_7 accept"));
        assert!(r.contains("add rule inet puku vm_7 drop"));
        assert!(r.contains("\"pkt7\" : jump vm_7"));
    }
}
