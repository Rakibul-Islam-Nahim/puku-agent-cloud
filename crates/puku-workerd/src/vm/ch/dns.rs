//! The resolver a Cloud Hypervisor guest is given: its gateway, port 53.
//!
//! In open mode it forwards everything. In allowlist mode it is the policy:
//! a name outside the allowlist gets NXDOMAIN (so the guest sees "could not
//! resolve", which is what msb's policy produces too), and every IPv4 answer
//! for an allowed name is added to that VM's nftables set for the record's
//! TTL. The forward chain admits only that set, so an address the guest
//! learned any other way -- hard-coded, or from a resolver it smuggled in
//! over an allowed host -- goes nowhere.
//!
//! Known approximation, the same one every DNS-based allowlist has: an
//! allowed name that shares an address with a disallowed one (a CDN) opens
//! that address.
//!
//! UDP only. Resolvers fall back to TCP for oversized answers, which here
//! means that lookup fails; allowlisted API and registry hosts do not answer
//! that large.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

use super::net::NetSlot;

#[derive(Debug, Clone)]
pub struct Policy {
    /// Domain suffixes; empty forwards everything.
    pub allow: Vec<String>,
    pub upstream: SocketAddr,
}

impl Policy {
    fn permits(&self, name: &str) -> bool {
        if self.allow.is_empty() {
            return true;
        }
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        self.allow.iter().any(|d| {
            let d = d.trim_start_matches('.').to_ascii_lowercase();
            name == d || name.ends_with(&format!(".{d}"))
        })
    }
}

/// The resolver the host itself uses, for forwarding.
pub fn host_upstream() -> SocketAddr {
    std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .and_then(|c| {
            c.lines()
                .filter_map(|l| l.strip_prefix("nameserver"))
                .filter_map(|ip| ip.trim().parse::<std::net::IpAddr>().ok())
                // systemd-resolved's stub is fine from the host; a loopback
                // address is still the host's own.
                .map(|ip| SocketAddr::new(ip, 53))
                .next()
        })
        .unwrap_or_else(|| SocketAddr::from(([1, 1, 1, 1], 53)))
}

/// Serve DNS for one VM on its gateway address until aborted.
pub fn spawn(slot: NetSlot, policy: Policy) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let bind = SocketAddr::from((slot.host, 53));
        let sock = match UdpSocket::bind(bind).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(tap = %slot.tap, error = %e, "the VM's resolver could not bind");
                return;
            }
        };
        let mut buf = vec![0u8; 1500];
        loop {
            let Ok((n, client)) = sock.recv_from(&mut buf).await else { continue };
            let query = buf[..n].to_vec();
            let (sock, slot, policy) = (sock.clone(), slot.clone(), policy.clone());
            tokio::spawn(async move {
                if let Some(reply) = answer(&query, &slot, &policy).await {
                    let _ = sock.send_to(&reply, client).await;
                }
            });
        }
    })
}

async fn answer(query: &[u8], slot: &NetSlot, policy: &Policy) -> Option<Vec<u8>> {
    let name = question_name(query)?;
    if !policy.permits(&name) {
        return Some(nxdomain(query));
    }
    let up = UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
    up.send_to(query, policy.upstream).await.ok()?;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(3), up.recv(&mut buf)).await.ok()?.ok()?;
    let reply = buf[..n].to_vec();
    if !policy.allow.is_empty() {
        for (ip, ttl) in a_records(&reply) {
            if let Err(e) = super::net::allow(slot, ip, ttl) {
                tracing::warn!(tap = %slot.tap, %ip, error = format!("{e:#}"), "admitting a resolved address failed");
            }
        }
    }
    Some(reply)
}

/// Skip a (possibly compressed) name starting at `i`; the offset after it.
fn skip_name(msg: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let len = *msg.get(i)? as usize;
        if len & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        if len == 0 {
            return Some(i + 1);
        }
        i += 1 + len;
    }
}

/// The first question's name, lower-cased, without the trailing dot.
pub fn question_name(msg: &[u8]) -> Option<String> {
    if msg.len() < 12 || u16::from_be_bytes([msg[4], msg[5]]) == 0 {
        return None;
    }
    let mut i = 12;
    let mut labels = Vec::new();
    loop {
        let len = *msg.get(i)? as usize;
        if len == 0 {
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compression in a question: not something a stub sends
        }
        labels.push(std::str::from_utf8(msg.get(i + 1..i + 1 + len)?).ok()?.to_ascii_lowercase());
        i += 1 + len;
    }
    Some(labels.join("."))
}

/// NXDOMAIN for `query`: same id and question, response bit set, no answers.
pub fn nxdomain(query: &[u8]) -> Vec<u8> {
    let q_end = skip_name(query, 12).map(|i| i + 4).unwrap_or(query.len()).min(query.len());
    let mut r = query[..q_end].to_vec();
    if r.len() >= 12 {
        r[2] = 0x80 | (query[2] & 0x01) | 0x04; // QR, keep RD, AA
        r[3] = 0x80 | 0x03; // RA, RCODE=3
        r[6..12].copy_from_slice(&[0, 0, 0, 0, 0, 0]); // no answer/authority/additional
    }
    r
}

/// Every A record in a response's answer section, with its TTL.
pub fn a_records(msg: &[u8]) -> Vec<(Ipv4Addr, u32)> {
    let mut out = Vec::new();
    if msg.len() < 12 {
        return out;
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut i = 12;
    for _ in 0..qd {
        match skip_name(msg, i) {
            Some(j) => i = j + 4,
            None => return out,
        }
    }
    for _ in 0..an {
        let Some(j) = skip_name(msg, i) else { return out };
        let Some(fixed) = msg.get(j..j + 10) else { return out };
        let rtype = u16::from_be_bytes([fixed[0], fixed[1]]);
        let ttl = u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]);
        let rdlen = u16::from_be_bytes([fixed[8], fixed[9]]) as usize;
        let rdata = j + 10;
        if rtype == 1 && rdlen == 4 {
            if let Some(b) = msg.get(rdata..rdata + 4) {
                out.push((Ipv4Addr::new(b[0], b[1], b[2], b[3]), ttl));
            }
        }
        i = rdata + rdlen;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query for `api.github.com` A, as `dig` sends it.
    fn query(name: &str) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        q
    }

    /// The reply an upstream gives: the question, then two A answers using
    /// a compression pointer back to it.
    fn reply(name: &str) -> Vec<u8> {
        let mut r = query(name);
        r[2] = 0x81;
        r[3] = 0x80;
        r[7] = 2;
        for (ip, ttl) in [([140, 82, 112, 5], 60u32), ([140, 82, 112, 6], 3600)] {
            r.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1]);
            r.extend_from_slice(&ttl.to_be_bytes());
            r.extend_from_slice(&[0, 4]);
            r.extend_from_slice(&ip);
        }
        r
    }

    #[test]
    fn the_question_is_read_lower_cased() {
        assert_eq!(question_name(&query("API.GitHub.com")).as_deref(), Some("api.github.com"));
        assert_eq!(question_name(&[0; 5]), None);
    }

    #[test]
    fn suffixes_match_on_label_boundaries_only() {
        let p = Policy { allow: vec!["github.com".into(), ".puku.sh".into()], upstream: host_upstream() };
        assert!(p.permits("github.com"));
        assert!(p.permits("api.github.com."));
        assert!(p.permits("api-cli.puku.sh"));
        assert!(!p.permits("evilgithub.com"), "a suffix is not a substring");
        assert!(!p.permits("github.com.evil.io"));
        let open = Policy { allow: vec![], upstream: host_upstream() };
        assert!(open.permits("anything.example"));
    }

    #[test]
    fn a_refusal_is_nxdomain_for_the_same_question() {
        let q = query("evil.example");
        let r = nxdomain(&q);
        assert_eq!(&r[..2], &q[..2], "same id");
        assert_eq!(r[3] & 0x0F, 3, "NXDOMAIN");
        assert_ne!(r[2] & 0x80, 0, "a response");
        assert_eq!(&r[6..8], &[0, 0], "no answers");
        assert_eq!(question_name(&r).as_deref(), Some("evil.example"));
    }

    #[test]
    fn answers_are_read_through_compression_pointers() {
        let got = a_records(&reply("api.github.com"));
        assert_eq!(
            got,
            vec![(Ipv4Addr::new(140, 82, 112, 5), 60), (Ipv4Addr::new(140, 82, 112, 6), 3600)]
        );
        assert!(a_records(&[1, 2, 3]).is_empty(), "garbage is empty, not a panic");
    }
}
