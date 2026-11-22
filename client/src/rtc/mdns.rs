//! Browser `.local` mDNS candidate resolution for faster LAN direct connections.
//!
//! Modern browsers obfuscate LAN host candidates as `<uuid>.local` (mDNS). When the computer's OS
//! firewall drops unsolicited inbound UDP packets, resolving `.local` lets libjuice send outbound STUN
//! checks directly to the browser's LAN IP, punching through stateful host firewalls on both sides.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;

const MDNS_PORT: u16 = 5353;
const MDNS_MCAST_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;
const QCLASS_IN_QU: u16 = 0x8001; // IN + unicast-response bit (RFC 6762 §5.4)

/// Extract a valid `.local` mDNS hostname from an ICE candidate SDP line.
pub fn extract_mdns_host(candidate_line: &str) -> Option<&str> {
    let trimmed = candidate_line.trim().trim_start_matches("a=");
    if !trimmed.starts_with("candidate:") {
        return None;
    }
    let addr = trimmed.split_whitespace().nth(4)?;
    let host = addr.strip_suffix('.').unwrap_or(addr);
    if host.len() <= 6 || host.len() > 128 {
        return None;
    }
    if !host.to_ascii_lowercase().ends_with(".local") {
        return None;
    }
    if host.starts_with('.') || host.contains("..") {
        return None;
    }
    if !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.') {
        return None;
    }
    Some(addr)
}

/// Build an RFC 6762 DNS query packet for `host` with the QU (unicast response) bit set.
pub fn build_mdns_query(host: &str, qtype: u16) -> Option<Vec<u8>> {
    let clean = host.trim_end_matches('.');
    if clean.is_empty() || clean.len() > 253 {
        return None;
    }
    let mut pkt = Vec::with_capacity(18 + clean.len());
    // Header: ID=0, Flags=0, QDCOUNT=1, ANCOUNT=0, NSCOUNT=0, ARCOUNT=0
    pkt.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in clean.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0);
    pkt.extend_from_slice(&qtype.to_be_bytes());
    pkt.extend_from_slice(&QCLASS_IN_QU.to_be_bytes());
    Some(pkt)
}

fn read_name(pkt: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut jumped_end = None;
    let mut jumps = 0;

    loop {
        if pos >= pkt.len() {
            return None;
        }
        let len = pkt[pos];
        if (len & 0xC0) == 0xC0 {
            if pos + 1 >= pkt.len() || jumps >= 16 {
                return None;
            }
            if jumped_end.is_none() {
                jumped_end = Some(pos + 2);
            }
            let ptr = (((len & 0x3F) as usize) << 8) | (pkt[pos + 1] as usize);
            pos = ptr;
            jumps += 1;
            continue;
        }
        if (len & 0xC0) != 0 {
            return None;
        }
        pos += 1;
        if len == 0 {
            break;
        }
        let n = len as usize;
        if pos + n > pkt.len() {
            return None;
        }
        let s = std::str::from_utf8(&pkt[pos..pos + n]).ok()?;
        labels.push(s.to_string());
        pos += n;
    }

    let next_pos = jumped_end.unwrap_or(pos);
    Some((labels.join("."), next_pos))
}

fn is_usable_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !v.is_unspecified()
                && !v.is_loopback()
                && !v.is_multicast()
                && !v.is_broadcast()
                && !v.is_link_local()
                && !(o[0] == 198 && (o[1] == 18 || o[1] == 19))
        }
        IpAddr::V6(v) => {
            let seg0 = v.segments()[0];
            !v.is_unspecified() && !v.is_loopback() && !v.is_multicast() && (seg0 & 0xFFC0) != 0xFE80
        }
    }
}

/// Parse an RFC 6762 mDNS response packet and extract matching A / AAAA LAN IPs for `expected_host`.
pub fn parse_mdns_response(pkt: &[u8], expected_host: &str) -> Vec<IpAddr> {
    if pkt.len() < 12 {
        return Vec::new();
    }
    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    if (flags & 0x8000) == 0 {
        return Vec::new();
    }
    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    let ancount = u16::from_be_bytes([pkt[6], pkt[7]]) as usize;
    let nscount = u16::from_be_bytes([pkt[8], pkt[9]]) as usize;
    let arcount = u16::from_be_bytes([pkt[10], pkt[11]]) as usize;

    let mut pos = 12;
    for _ in 0..qdcount {
        let Some((_, next)) = read_name(pkt, pos) else { return Vec::new() };
        pos = next + 4;
        if pos > pkt.len() {
            return Vec::new();
        }
    }

    let target = expected_host.trim_end_matches('.');
    let mut out = Vec::new();
    let total_rr = ancount.saturating_add(nscount).saturating_add(arcount);

    for _ in 0..total_rr {
        let Some((name, next)) = read_name(pkt, pos) else { break };
        if next + 10 > pkt.len() {
            break;
        }
        let rtype = u16::from_be_bytes([pkt[next], pkt[next + 1]]);
        let rdlen = u16::from_be_bytes([pkt[next + 8], pkt[next + 9]]) as usize;
        let rdata_start = next + 10;
        let rdata_end = rdata_start + rdlen;
        if rdata_end > pkt.len() {
            break;
        }
        pos = rdata_end;

        if !name.trim_end_matches('.').eq_ignore_ascii_case(target) {
            continue;
        }
        let ip = match (rtype, rdlen) {
            (QTYPE_A, 4) => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&pkt[rdata_start..rdata_end]);
                Some(IpAddr::V4(Ipv4Addr::from(b)))
            }
            (QTYPE_AAAA, 16) => {
                let mut b = [0u8; 16];
                b.copy_from_slice(&pkt[rdata_start..rdata_end]);
                Some(IpAddr::V6(Ipv6Addr::from(b)))
            }
            _ => None,
        };
        if let Some(ip) = ip.filter(|ip| is_usable_lan_ip(*ip)) {
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }

    sort_ips(&mut out);
    out
}

fn ip_rank(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v) if v.is_private() => 0,
        IpAddr::V4(_) => 1,
        IpAddr::V6(v) if (v.segments()[0] & 0xE000) == 0x2000 => 2,
        IpAddr::V6(_) => 3,
    }
}

fn sort_ips(ips: &mut [IpAddr]) {
    ips.sort_by_key(ip_rank);
}

fn skip_iface(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "docker", "br-", "veth", "virbr", "vmnet", "vboxnet", "utun", "tun", "tap", "Meta",
        "mihomo", "clash", "Clash", "bridge", "awdl", "llw", "anpi", "cni", "flannel", "kube",
        "lxc", "lxdbr", "podman", "lo",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

fn lan_bind_v4_addrs() -> Vec<Ipv4Addr> {
    let mut addrs = Vec::new();
    if let Ok(list) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in list {
            if skip_iface(&name) {
                continue;
            }
            if let IpAddr::V4(v4) = ip {
                if is_usable_lan_ip(IpAddr::V4(v4)) && !addrs.contains(&v4) {
                    addrs.push(v4);
                }
            }
        }
    }
    addrs.sort_by_key(|v| if v.is_private() { 0u8 } else { 1 });
    addrs.truncate(4);
    addrs
}

async fn query_via_multicast_udp(host: &str, allow_ipv6: bool, budget: Duration) -> Vec<IpAddr> {
    let Some(pkt_a) = build_mdns_query(host, QTYPE_A) else {
        return Vec::new();
    };
    let pkt_aaaa = allow_ipv6.then(|| build_mdns_query(host, QTYPE_AAAA)).flatten();
    let dst = SocketAddr::V4(SocketAddrV4::new(MDNS_MCAST_V4, MDNS_PORT));

    let mut bind_ips = lan_bind_v4_addrs();
    if bind_ips.is_empty() {
        bind_ips.push(Ipv4Addr::UNSPECIFIED);
    }

    let mut sockets = Vec::new();
    for ip in bind_ips {
        if let Ok(sock) = UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(ip, 0))).await {
            let _ = sock.set_multicast_ttl_v4(255);
            sockets.push(sock);
        }
    }
    if sockets.is_empty() {
        return Vec::new();
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<IpAddr>>();
    let mut set = tokio::task::JoinSet::new();
    let deadline = tokio::time::Instant::now() + budget;
    let host_owned = host.to_string();
    for sock in sockets {
        let _ = sock.send_to(&pkt_a, dst).await;
        if let Some(ref p6) = pkt_aaaa {
            let _ = sock.send_to(p6, dst).await;
        }
        let tx = tx.clone();
        let pkt_a_retry = pkt_a.clone();
        let pkt_aaaa_retry = pkt_aaaa.clone();
        let expected = host_owned.clone();
        set.spawn(async move {
            let mut buf = [0u8; 1500];
            let mut retried = false;
            let retry_at = tokio::time::sleep(Duration::from_millis(200));
            let stop = tokio::time::sleep_until(deadline);
            tokio::pin!(retry_at);
            tokio::pin!(stop);
            loop {
                tokio::select! {
                    res = sock.recv_from(&mut buf) => {
                        let Ok((n, _)) = res else { break };
                        let ips = parse_mdns_response(&buf[..n], &expected);
                        if !ips.is_empty() && tx.send(ips).is_err() {
                            break;
                        }
                    }
                    _ = &mut retry_at, if !retried => {
                        retried = true;
                        let _ = sock.send_to(&pkt_a_retry, dst).await;
                        if let Some(ref p6) = pkt_aaaa_retry {
                            let _ = sock.send_to(p6, dst).await;
                        }
                    }
                    _ = &mut stop => break,
                }
            }
        });
    }
    drop(tx);

    let mut collected = Vec::new();
    let mut v6_grace: Option<tokio::time::Instant> = None;

    loop {
        let stop_at = v6_grace.map_or(deadline, |g| g.min(deadline));
        let now = tokio::time::Instant::now();
        if now >= stop_at {
            break;
        }
        match tokio::time::timeout(stop_at - now, rx.recv()).await {
            Ok(Some(batch)) => {
                for ip in batch {
                    if !allow_ipv6 && ip.is_ipv6() {
                        continue;
                    }
                    if !collected.contains(&ip) {
                        collected.push(ip);
                    }
                }
                if collected.iter().any(|ip| ip.is_ipv4()) {
                    break;
                }
                if !collected.is_empty() && v6_grace.is_none() {
                    v6_grace = Some(tokio::time::Instant::now() + Duration::from_millis(60));
                }
            }
            _ => break,
        }
    }

    set.shutdown().await;
    sort_ips(&mut collected);
    collected
}

async fn resolve_one(host: &str, allow_ipv6: bool, budget: Duration) -> Vec<IpAddr> {
    query_via_multicast_udp(host, allow_ipv6, budget).await
}

fn rewrite_candidate_line(line: &str, host: &str, ip: IpAddr) -> String {
    let mut out = String::with_capacity(line.len() + 32);
    let mut idx = 0usize;
    let mut replaced = false;
    for part in line.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        if idx == 4 && part == host && !replaced {
            out.push_str(&ip.to_string());
            replaced = true;
        } else {
            out.push_str(part);
        }
        idx += 1;
    }
    out
}

/// Resolve any `.local` mDNS hostnames in deferred `(mid, candidate)` pairs and replace them
/// with the resolved LAN IP(s), preserving all other candidate attributes.
pub async fn resolve_candidates(
    candidates: Vec<(String, String)>,
    allow_ipv6: bool,
    budget: Duration,
) -> Vec<(String, String)> {
    let mut hosts = Vec::new();
    let mut seen = HashSet::new();
    for (_, line) in &candidates {
        if let Some(h) = extract_mdns_host(line) {
            let key = h.to_ascii_lowercase();
            if seen.insert(key) {
                hosts.push(h.to_string());
                if hosts.len() >= 8 {
                    break;
                }
            }
        }
    }
    if hosts.is_empty() {
        return candidates;
    }

    let resolved_lists = futures_util::future::join_all(
        hosts.iter().map(|h| resolve_one(h, allow_ipv6, budget)),
    )
    .await;

    let mut map = std::collections::HashMap::new();
    for (host, mut ips) in hosts.into_iter().zip(resolved_lists) {
        if !ips.is_empty() {
            // Keep at most best IPv4 + best IPv6 per hostname
            let mut chosen = Vec::new();
            if let Some(v4) = ips.iter().find(|ip| ip.is_ipv4()).copied() {
                chosen.push(v4);
            }
            if allow_ipv6 {
                if let Some(v6) = ips.drain(..).find(|ip| ip.is_ipv6()) {
                    chosen.push(v6);
                }
            }
            if !chosen.is_empty() {
                map.insert(host.to_ascii_lowercase(), chosen);
            }
        }
    }

    if map.is_empty() {
        return candidates;
    }

    let mut out = Vec::with_capacity(candidates.len() + map.len());
    for (mid, line) in candidates {
        if let Some(host) = extract_mdns_host(&line) {
            if let Some(ips) = map.get(&host.to_ascii_lowercase()) {
                for &ip in ips {
                    out.push((mid.clone(), rewrite_candidate_line(&line, host, ip)));
                }
                continue;
            }
        }
        out.push((mid, line));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_name(out: &mut Vec<u8>, name: &str) {
        for label in name.trim_end_matches('.').split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
    }

    #[test]
    fn query_and_parse_compressed_response() {
        let host = "f47ac10b-58cc-4372-a567-0e02b2c3d479.local";
        let q = build_mdns_query(host, QTYPE_A).unwrap();
        assert_eq!(&q[0..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        let tail = &q[q.len() - 4..];
        assert_eq!(tail, &[0, 1, 0x80, 0x01]);

        // Build synthetic DNS response with 4 answer RRs using name compression pointer 0xC0 0x0C
        let mut resp = Vec::new();
        resp.extend_from_slice(&[0, 0, 0x84, 0x00]); // QR=1, AA=1
        resp.extend_from_slice(&[0, 1, 0, 4, 0, 0, 0, 0]); // QD=1, AN=4
        let qname_off = resp.len();
        encode_name(&mut resp, host);
        resp.extend_from_slice(&QTYPE_A.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes());

        let ptr = 0xC000u16 | (qname_off as u16);
        // 1. Unusable Clash fake-ip 198.18.0.5
        resp.extend_from_slice(&ptr.to_be_bytes());
        resp.extend_from_slice(&QTYPE_A.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes());
        resp.extend_from_slice(&120u32.to_be_bytes());
        resp.extend_from_slice(&4u16.to_be_bytes());
        resp.extend_from_slice(&[198, 18, 0, 5]);
        // 2. Unusable loopback 127.0.0.1
        resp.extend_from_slice(&ptr.to_be_bytes());
        resp.extend_from_slice(&QTYPE_A.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes());
        resp.extend_from_slice(&120u32.to_be_bytes());
        resp.extend_from_slice(&4u16.to_be_bytes());
        resp.extend_from_slice(&[127, 0, 0, 1]);
        // 3. Valid ULA IPv6 fd00::2
        resp.extend_from_slice(&ptr.to_be_bytes());
        resp.extend_from_slice(&QTYPE_AAAA.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes());
        resp.extend_from_slice(&120u32.to_be_bytes());
        resp.extend_from_slice(&16u16.to_be_bytes());
        resp.extend_from_slice(&Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2).octets());
        // 4. Valid RFC1918 IPv4 192.168.1.42
        resp.extend_from_slice(&ptr.to_be_bytes());
        resp.extend_from_slice(&QTYPE_A.to_be_bytes());
        resp.extend_from_slice(&1u16.to_be_bytes());
        resp.extend_from_slice(&120u32.to_be_bytes());
        resp.extend_from_slice(&4u16.to_be_bytes());
        resp.extend_from_slice(&[192, 168, 1, 42]);

        let ips = parse_mdns_response(&resp, host);
        assert_eq!(
            ips,
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
                IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2)),
            ]
        );
    }

    #[test]
    fn rewrite_candidate_preserves_fields() {
        let host = "12345678-abcd-ef00-1122-334455667788.local";
        let line = format!("a=candidate:12345 1 udp 2122260223 {host} 54321 typ host generation 0 network-cost 10");
        assert_eq!(extract_mdns_host(&line), Some(host));
        assert_eq!(
            extract_mdns_host("a=candidate:12345 1 udp 2122260223 192.168.1.2 54321 typ host"),
            None
        );
        assert_eq!(
            extract_mdns_host("a=candidate:12345 1 udp 2122260223 bad..name.local 54321 typ host"),
            None
        );
        let rewritten = rewrite_candidate_line(&line, host, IpAddr::V4(Ipv4Addr::new(192, 168, 31, 88)));
        assert_eq!(
            rewritten,
            "a=candidate:12345 1 udp 2122260223 192.168.31.88 54321 typ host generation 0 network-cost 10"
        );
    }

    #[tokio::test]
    async fn udp_roundtrip_mock_responder() {
        let host = "aabbccdd-1122-3344-5566-778899001122.local";
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();

        let srv_task = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            assert!(n > 16);
            let mut resp = Vec::new();
            resp.extend_from_slice(&[0, 0, 0x84, 0x00, 0, 0, 0, 1, 0, 0, 0, 0]);
            encode_name(&mut resp, host);
            resp.extend_from_slice(&QTYPE_A.to_be_bytes());
            resp.extend_from_slice(&1u16.to_be_bytes());
            resp.extend_from_slice(&120u32.to_be_bytes());
            resp.extend_from_slice(&4u16.to_be_bytes());
            resp.extend_from_slice(&[10, 0, 0, 99]);
            server.send_to(&resp, peer).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let q = build_mdns_query(host, QTYPE_A).unwrap();
        client.send_to(&q, addr).await.unwrap();
        let mut buf = [0u8; 512];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let ips = parse_mdns_response(&buf[..n], host);
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99))]);
        srv_task.await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unresolvable_mdns_does_not_leak_fds() {
        let count_socks = || -> usize {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
                .filter(|p| p.to_string_lossy().starts_with("socket:"))
                .count()
        };
        let cands = vec![(
            "0".to_string(),
            "a=candidate:1 1 udp 2122260223 nonexistent-leak-test.local 54321 typ host".to_string(),
        )];
        // Warm up once before baseline
        let _ = resolve_candidates(cands.clone(), false, Duration::from_millis(50)).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = count_socks();
        for _ in 0..8 {
            // Cancel mid-flight and also let expire cleanly
            let _ = tokio::time::timeout(
                Duration::from_millis(25),
                resolve_candidates(cands.clone(), false, Duration::from_millis(200)),
            )
            .await;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        let after = count_socks();
        assert!(after <= before + 3, "mDNS sockets leaked: before={before}, after={after}");
    }
}
