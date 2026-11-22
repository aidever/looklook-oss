//! 直连自检（服务端 docs/08）：
//!
//! - **B. NAT 映射类型**（§4）：从端口段里一个空闲的本地端口，分别问两台不同 IP 的中转的 STUN，比较两边看到的映射
//!   `IP:端口`。线路列表里不同 IP 的中转不到两台时用公共 STUN（[`PUBLIC_STUN`]）补；
//! - **A. 端口可达**（§3）：在端口段首、尾各找一个**还没往外发过包**的空闲端口，请主服务往“公网 IPv4:端口”各发几个
//!   UDP 包（`POST /api/client/v1/rtc/reach`），3 秒内收到就是从外面能打进来。目标是填的公网 IPv4，没填就用 B 测出的出口。
//!   B 先做（用第三个端口），A 的两个端口在检测前没有发过包，NAT / 防火墙里没有放行记录，收到包才可信。
//!
//! 分档（§5，按顺序）：
//!
//! - 中转域名解析成 / 映射出 `198.18.0.0/15`，或映射 IP 与各网站查到的出口不一致：代理软件在接管，测不准（`proxy`）；
//! - A 首尾都通：`open`（没填公网地址时给出 `suggest`，界面上点了才填）；只通一个：`partial`；
//! - 填了公网 IPv4，但它和映射出的出口 IP 不一致（运营商 NAT、IP 变了）、域名还没解析出来，或一致但 A 不通：`wrong_addr`；
//! - B 一样：端点无关映射，多数网络能打洞直连（`punch`）；
//! - 不一样：对称 NAT，对方也在严格 NAT 后面时只能中转（`relay_only`）；
//! - 中转域名解析成 / 映射出 `198.18.0.0/15`，或映射 IP 与各网站查到的出口不一致：代理软件在接管，测不准（`proxy`）；
//! - 其余（没登录、只有一台中转、STUN 都没回）：`unknown`。
//!
//! 只测 IPv4 UDP。主服务没有这个接口（旧版，404）或被限流时 A 记“没测”（`a: skipped`），按 B 分档。不定时反复跑：启动后 30 秒、改了端口范围或
//! 公网地址后、设置页“检测”、`looklook rtc check` 各跑一次。结论存本机 kv（`rtc_reach`），随心跳上报平台。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use looklook_protocol::dto::RtcReach;
use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::store::Store;

const KEY: &str = "rtc_reach";
/// 启动后多久跑第一次（等线路、DDNS 解析就绪）。
const FIRST_DELAY: Duration = Duration::from_secs(30);
/// 中转不够两台时补用的公共 STUN（国内、国外各一个）。
const PUBLIC_STUN: &[&str] = &["stun.miwifi.com:3478", "stun.cloudflare.com:3478"];

/// 问一台中转的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub relay: String,
    /// 中转 STUN 解析出的地址
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SocketAddr>,
    /// 它看到的这台电脑的 `IP:端口`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapped: Option<SocketAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A 里一个端口的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortProbe {
    pub port: u16,
    pub open: bool,
}

/// A 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reach {
    /// `both` / `one` / `none` / `skipped`
    pub a: String,
    /// 没测的原因：`not_logged_in` / `ports_busy`（端口段里空闲端口不够）/ `no_target`（没填地址，B 也没测出公网出口）/
    /// `unsupported`（主服务是旧版）/ `limited`（每小时 6 次用完了）/ `error`
    pub why: Option<String>,
    /// 测的地址
    pub target: Option<IpAddr>,
    pub ports: Vec<PortProbe>,
}

impl Reach {
    pub fn skipped(why: &str) -> Reach {
        Reach { a: "skipped".into(), why: Some(why.into()), target: None, ports: Vec::new() }
    }
}

fn skipped() -> String {
    "skipped".into()
}

/// 一次检测的结论（存 kv，接口与命令行原样给出）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// `proxy` / `open` / `partial` / `wrong_addr` / `punch` / `relay_only` / `unknown`
    pub verdict: String,
    /// A：`both` / `one` / `none` / `skipped`（旧版存的结论没有，按 skipped）
    #[serde(default = "skipped")]
    pub a: String,
    /// A 没测的原因（[`Reach::why`]）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a_why: Option<String>,
    /// A 测的地址
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a_target: Option<IpAddr>,
    /// A 测的端口
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortProbe>,
    /// 没填公网地址、A 首尾都通（云主机）：建议填入的地址，界面上点了才填
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggest: Option<IpAddr>,
    /// `eim`（端点无关映射）/ `symmetric` / `proxy` / `skipped`
    pub nat: String,
    /// 补充原因：`proxy` 时 `fake_ip` / `exit_mismatch`；`wrong_addr` 时 `mismatch` / `unresolved` / `blocked`（一致但打不进来）；
    /// `unknown` 时 `not_logged_in` / `one_relay` / `no_reply`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Unix 秒
    pub at: i64,
    /// B 用的本地端口；`in_range` 为 false 时端口段全被占用，用的是随机端口
    pub local_port: u16,
    pub in_range: bool,
    pub mappings: Vec<Mapping>,
    /// 映射出的出口 IP（两台一致时）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<IpAddr>,
    /// 填的公网 IPv4（域名时是解析结果；没填为 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_v4: Option<IpAddr>,
}

impl Report {
    /// 心跳上报的样子（docs/08 §8）。
    pub fn brief(&self) -> RtcReach {
        RtcReach { verdict: self.verdict.clone(), at: self.at, a: self.a.clone(), b: self.nat.clone() }
    }
}

static LAST: Mutex<Option<Report>> = Mutex::new(None);

/// 最近一次的结论（启动时从 kv 读出）。
pub fn last() -> Option<Report> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 心跳里带的结论。
pub fn reported() -> Option<RtcReach> {
    last().map(|r| r.brief())
}

pub fn load(store: &Store) {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = store.get::<Report>(KEY).ok().flatten();
}

fn save(store: &Store, r: &Report) {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(r.clone());
    if let Err(e) = store.set(KEY, r) {
        tracing::warn!(error = %e, "NAT 检测结论没存上");
    }
}

/// 跑一次检测并保存。`site_exit`：各网站查到的出口 IPv4（设置页“检测”时一起查，自动检测时不查）。
pub async fn check(account: &Account, store: &Store, site_exit: Option<IpAddr>) -> Report {
    let cfg = super::net::current();
    let filled = cfg.public_v4.as_deref().map(|s| super::net::public_ip(s, false));
    let at = crate::util::system_ms() / 1000;
    let socks = bind(cfg.port_begin, cfg.port_end).await;
    let local_port = socks.b.as_ref().and_then(|s| s.local_addr().ok()).map_or(0, |a| a.port());
    let in_range = socks.in_range;
    let r = if account.session().is_none() {
        let mut r = classify(Vec::new(), None, None, Reach::skipped("not_logged_in"), at, local_port, in_range);
        r.why = Some("not_logged_in".into());
        r.public_v4 = None;
        r
    } else {
        // B 先做（第三个端口），拿到出口地址；A 的两个端口这之前没发过包
        let mappings = match &socks.b {
            Some(sock) => ask(account, sock).await,
            None => Vec::new(),
        };
        let exit = same_ip(&mappings);
        let target = match filled {
            Some(Some(ip)) if !non_global(ip) => Some(ip),
            _ => exit.filter(|ip| !non_global(*ip) && !fake_ip(*ip)),
        };
        let a = match (socks.a.is_empty(), target) {
            (true, _) => Reach::skipped("ports_busy"),
            (false, None) => Reach::skipped("no_target"),
            (false, Some(ip)) => probe(account, &socks.a, ip).await,
        };
        classify(mappings, filled, site_exit, a, at, local_port, in_range)
    };
    tracing::info!(verdict = %r.verdict, a = %r.a, a_why = ?r.a_why, nat = %r.nat, why = ?r.why, port = local_port, ports = ?r.ports, mappings = ?r.mappings, "直连自检");
    save(store, &r);
    r
}

/// 检测用的 socket：`a` 是端口段首、尾两个空闲端口（A 用；端口段里只空一个时只有一个），`b` 是 B 用的第三个
/// （从尾往前找；不够时用随机端口，`in_range` 为 false）。
struct Socks {
    a: Vec<tokio::net::UdpSocket>,
    b: Option<tokio::net::UdpSocket>,
    in_range: bool,
}

/// PeerConnection 从端口段头往后用，这里只拿当前空闲的，不碰正在用的。
async fn bind(begin: u16, end: u16) -> Socks {
    async fn first(ports: impl Iterator<Item = u16>, skip: &[u16]) -> Option<(u16, tokio::net::UdpSocket)> {
        for p in ports {
            if skip.contains(&p) {
                continue;
            }
            if let Ok(s) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, p)).await {
                return Some((p, s));
            }
        }
        None
    }
    let head = first(begin..=end, &[]).await;
    let used: Vec<u16> = head.iter().map(|(p, _)| *p).collect();
    let tail = first((begin..=end).rev(), &used).await;
    let used: Vec<u16> = head.iter().chain(tail.iter()).map(|(p, _)| *p).collect();
    let b = first((begin..=end).rev(), &used).await;
    let a: Vec<_> = head.into_iter().chain(tail).map(|(_, s)| s).collect();
    match b {
        Some((_, s)) => Socks { a, b: Some(s), in_range: true },
        None => Socks { a, b: tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok(), in_range: false },
    }
}

/// 等主服务发来的包（docs/08 §3.1：3 个包、间隔 300 ms），从接口回 202 起最多等这么久。
const A_WAIT: Duration = Duration::from_secs(3);

/// A：请主服务往 `ip` 的这几个端口发包，看各自收没收到。
async fn probe(account: &Account, socks: &[tokio::net::UdpSocket], ip: IpAddr) -> Reach {
    use looklook_protocol::rtc::{reach_packet, ReachRequest, ReachTarget};
    let nonce: [u8; 16] = rand::random();
    let ports: Vec<u16> = socks.iter().filter_map(|s| s.local_addr().ok()).map(|a| a.port()).collect();
    let req = ReachRequest { nonce: nonce.iter().map(|b| format!("{b:02x}")).collect(), targets: ports.iter().map(|p| ReachTarget { ip: ip.to_string(), port: *p }).collect() };
    let why = match account.rtc_reach(&req).await {
        None => Some("not_logged_in"),
        Some(Ok(_)) => None,
        Some(Err(crate::platform::PlatformError::Api { status: 404 | 405, .. })) => Some("unsupported"),
        Some(Err(crate::platform::PlatformError::Api { status: 429, .. })) => Some("limited"),
        Some(Err(e)) => {
            tracing::debug!(error = %e, "端口可达检测：请求主服务失败");
            Some("error")
        }
    };
    if let Some(w) = why {
        let mut r = Reach::skipped(w);
        r.target = Some(ip);
        return r;
    }
    let want = reach_packet(&nonce);
    let deadline = tokio::time::Instant::now() + A_WAIT;
    let got = futures_util::future::join_all(socks.iter().map(|s| async move {
        let mut buf = [0u8; 64];
        loop {
            match tokio::time::timeout_at(deadline, s.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) if buf[..n] == want => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }))
    .await;
    let n = got.iter().filter(|x| **x).count();
    let a = if n == 0 {
        "none"
    } else if n == got.len() {
        "both"
    } else {
        "one"
    };
    Reach { a: a.into(), why: None, target: Some(ip), ports: ports.into_iter().zip(got).map(|(port, open)| PortProbe { port, open }).collect() }
}

/// 依次问各中转（同一个 socket 不能同时等两个响应）；IP 相同的中转只问第一台，最多问两台不同 IP 的。
async fn ask(account: &Account, sock: &tokio::net::UdpSocket) -> Vec<Mapping> {
    let list = match account.relays().await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::debug!(error = %e, "NAT 检测：取线路列表失败");
            return Vec::new();
        }
    };
    // 线路列表里不同 IP 的中转不到两台（会员档位只给一台等）时，用公共 STUN 补：只问一次映射地址，不经过任何流量
    let relays = list.into_iter().filter_map(|i| Some((i.name, i.stun?)));
    let public = PUBLIC_STUN.iter().map(|s| (s.rsplit_once(':').map_or(*s, |(h, _)| h).to_string(), s.to_string()));
    let mut out: Vec<Mapping> = Vec::new();
    for (name, stun) in relays.chain(public) {
        if out.iter().filter(|m| m.mapped.is_some()).count() >= 2 {
            break;
        }
        let target = match crate::relay::resolve(&stun).await {
            Ok(t) if t.is_ipv4() => t,
            Ok(_) => continue,
            Err(e) => {
                out.push(Mapping { relay: name, target: None, mapped: None, error: Some(e) });
                continue;
            }
        };
        if out.iter().any(|m| m.target.is_some_and(|t| t.ip() == target.ip())) {
            continue;
        }
        let (mapped, error) = match crate::relay::stun_mapped_on(sock, target).await {
            Ok(a) => (Some(a), None),
            Err(e) => (None, Some(e)),
        };
        out.push(Mapping { relay: name, target: Some(target), mapped, error });
    }
    out
}

/// 代理软件 fake-ip 模式与 TUN 网卡常用的 198.18.0.0/15。
fn fake_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v) if v.octets()[0] == 198 && (v.octets()[1] & 0xfe) == 18)
}

/// 私有、CGNAT（Tailscale）这类地址：填了是有意组网用的，不和公网出口比。
fn non_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || (v.octets()[0] == 100 && (v.octets()[1] & 0xc0) == 64),
        IpAddr::V6(_) => true,
    }
}

/// 各中转映射出的 IP 都一样时的出口 IP。
fn same_ip(mappings: &[Mapping]) -> Option<IpAddr> {
    let ok: Vec<SocketAddr> = mappings.iter().filter_map(|m| m.mapped).collect();
    match ok.as_slice() {
        [a, rest @ ..] if rest.iter().all(|b| b.ip() == a.ip()) => Some(a.ip()),
        _ => None,
    }
}

/// 分档（docs/08 §5）。`filled`：没填为 None，填了域名还没解析出来为 Some(None)。
fn classify(mappings: Vec<Mapping>, filled: Option<Option<IpAddr>>, site_exit: Option<IpAddr>, a: Reach, at: i64, local_port: u16, in_range: bool) -> Report {
    let ok: Vec<SocketAddr> = mappings.iter().filter_map(|m| m.mapped).collect();
    let exit = same_ip(&mappings);
    let public_v4 = filled.flatten();
    let done = |verdict: &str, nat: &str, why: Option<&str>| Report {
        verdict: verdict.into(),
        a: a.a.clone(),
        a_why: a.why.clone(),
        a_target: a.target,
        ports: a.ports.clone(),
        suggest: (verdict == "open" && filled.is_none()).then_some(a.target).flatten(),
        nat: nat.into(),
        why: why.map(str::to_string),
        at,
        local_port,
        in_range,
        mappings: mappings.clone(),
        exit,
        public_v4,
    };
    if mappings.iter().any(|m| m.target.is_some_and(|t| fake_ip(t.ip())) || m.mapped.is_some_and(|a| fake_ip(a.ip()))) {
        return done("proxy", "proxy", Some("fake_ip"));
    }
    if let (Some(e), Some(s)) = (exit, site_exit) {
        if e != s {
            return done("proxy", "proxy", Some("exit_mismatch"));
        }
    }
    let nat = match ok.as_slice() {
        [a, b, ..] if a == b => "eim",
        [_, _, ..] => "symmetric",
        _ => "skipped",
    };
    match a.a.as_str() {
        "both" => return done("open", nat, None),
        "one" => return done("partial", nat, None),
        _ => {}
    }
    match filled {
        Some(None) => return done("wrong_addr", nat, Some("unresolved")),
        Some(Some(ip)) if !non_global(ip) && exit.is_some_and(|e| e != ip) => return done("wrong_addr", nat, Some("mismatch")),
        // 和出口一致（或 B 没测出），但从外面打不进来：端口没放行 / 转发没生效
        Some(Some(ip)) if !non_global(ip) && a.a == "none" => return done("wrong_addr", nat, Some("blocked")),
        _ => {}
    }
    match nat {
        "eim" => done("punch", nat, None),
        "symmetric" => done("relay_only", nat, None),
        // 能问的不同 IP 中转不到两台，还是问了但没回
        _ => done("unknown", nat, Some(if mappings.iter().filter(|m| m.target.is_some()).count() < 2 { "one_relay" } else { "no_reply" })),
    }
}

/// 启动后等登录、等 30 秒跑一次。
pub async fn startup(account: std::sync::Arc<Account>, store: std::sync::Arc<Store>) {
    tokio::time::sleep(FIRST_DELAY).await;
    while account.session().is_none() {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    check(&account, &store, None).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(relay: &str, target: &str, mapped: Option<&str>) -> Mapping {
        Mapping { relay: relay.into(), target: Some(target.parse().unwrap()), mapped: mapped.map(|s| s.parse().unwrap()), error: mapped.is_none().then(|| "timeout".into()) }
    }

    fn verdict(ms: Vec<Mapping>, filled: Option<Option<&str>>, site: Option<&str>) -> (String, String, Option<String>) {
        let r = classify(ms, filled.map(|f| f.map(|s| s.parse().unwrap())), site.map(|s| s.parse().unwrap()), Reach::skipped("unsupported"), 0, 50031, true);
        (r.verdict, r.nat, r.why)
    }

    fn reach(a: &str) -> Reach {
        let open = |i: usize| a == "both" || (a == "one" && i == 0);
        Reach { a: a.into(), why: None, target: Some("203.0.113.7".parse().unwrap()), ports: (0..2).map(|i| PortProbe { port: 50000 + i as u16 * 30, open: open(i) }).collect() }
    }

    fn with_a(ms: Vec<Mapping>, filled: Option<Option<&str>>, a: &str) -> Report {
        classify(ms, filled.map(|f| f.map(|s| s.parse().unwrap())), None, reach(a), 0, 50031, true)
    }

    #[test]
    fn classifies_with_reach() {
        let same = || vec![m("r1", R1, Some("203.0.113.7:50031")), m("r3", R2, Some("203.0.113.7:50031"))];
        let sym = || vec![m("r1", R1, Some("203.0.113.7:61000")), m("r3", R2, Some("203.0.113.7:61001"))];
        let r = with_a(same(), None, "both");
        assert_eq!((r.verdict.as_str(), r.a.as_str()), ("open", "both"));
        assert_eq!(r.suggest, Some("203.0.113.7".parse().unwrap()), "没填地址、首尾都通：建议填入");
        assert_eq!(r.brief().a, "both");
        let r = with_a(same(), Some(Some("203.0.113.7")), "both");
        assert_eq!((r.verdict.as_str(), r.suggest), ("open", None), "填过了不再建议");
        assert_eq!(with_a(sym(), None, "both").verdict, "open", "对称 NAT 但端口放行了：照样能直连");
        assert_eq!(with_a(same(), None, "one").verdict, "partial");
        assert_eq!(with_a(same(), Some(Some("203.0.113.7")), "one").verdict, "partial");
        let r = with_a(same(), Some(Some("203.0.113.7")), "none");
        assert_eq!((r.verdict.as_str(), r.why.as_deref()), ("wrong_addr", Some("blocked")));
        assert_eq!(with_a(same(), Some(Some("117.174.0.9")), "none").why.as_deref(), Some("mismatch"));
        assert_eq!(with_a(same(), None, "none").verdict, "punch", "没填地址、打不进来：按 B");
        assert_eq!(with_a(sym(), None, "none").verdict, "relay_only");
        assert_eq!(with_a(same(), Some(Some("100.101.102.103")), "none").verdict, "punch", "组网地址不算填错");
        // 代理优先
        let tun = vec![m("r1", R1, Some("198.18.0.1:50031")), m("r3", R2, Some("198.18.0.1:50031"))];
        assert_eq!(with_a(tun, None, "both").verdict, "proxy");
        // A 没测：和以前一样
        assert_eq!(verdict(same(), Some(Some("203.0.113.7")), None).0, "punch");
    }

    /// 旧版存的结论（没有 A 的字段）还能读出来。
    #[test]
    fn reads_old_report() {
        let old = serde_json::json!({ "verdict": "punch", "nat": "eim", "at": 1, "local_port": 50031, "in_range": true, "mappings": [] });
        let r: Report = serde_json::from_value(old).unwrap();
        assert_eq!((r.a.as_str(), r.ports.len()), ("skipped", 0));
    }

    /// 端口段首、尾各一个给 A，第三个给 B；被占用的跳过。
    #[tokio::test]
    async fn binds_three_ports() {
        let busy = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let p = busy.local_addr().unwrap().port();
        // 以被占用的端口为头的一小段：头被占用，往后找
        let (begin, end) = (p, p.saturating_add(9));
        let s = bind(begin, end).await;
        let a: Vec<u16> = s.a.iter().map(|x| x.local_addr().unwrap().port()).collect();
        let b = s.b.as_ref().unwrap().local_addr().unwrap().port();
        assert_eq!(a.len(), 2);
        assert!(!a.contains(&p) && b != p);
        assert!(a[0] < a[1] && !a.contains(&b));
        assert!(s.in_range && (begin..=end).contains(&b));
        // 只有一个端口的段：A 一个，B 随机
        drop(s);
        let s = bind(end, end).await;
        assert_eq!(s.a.len(), 1);
        assert!(!s.in_range);
    }

    const R1: &str = "34.0.0.1:3478";
    const R2: &str = "35.0.0.2:3478";

    #[test]
    fn classifies() {
        let same = || vec![m("r1", R1, Some("203.0.113.7:50031")), m("r3", R2, Some("203.0.113.7:50031"))];
        let sym = || vec![m("r1", R1, Some("203.0.113.7:61000")), m("r3", R2, Some("203.0.113.7:61001"))];
        let v = |a: &str, b: &str, w: Option<&str>| (a.to_string(), b.to_string(), w.map(str::to_string));
        assert_eq!(verdict(same(), None, None), v("punch", "eim", None));
        assert_eq!(verdict(sym(), None, None), v("relay_only", "symmetric", None));
        // 映射 IP 不同也算对称
        let ip_diff = vec![m("r1", R1, Some("203.0.113.7:50031")), m("r3", R2, Some("203.0.113.8:50031"))];
        assert_eq!(verdict(ip_diff, None, None).1, "symmetric");
        // 代理
        let fake = vec![m("r1", "198.18.0.5:3478", Some("203.0.113.7:50031")), m("r3", R2, Some("203.0.113.7:50031"))];
        assert_eq!(verdict(fake, None, None), v("proxy", "proxy", Some("fake_ip")));
        let tun = vec![m("r1", R1, Some("198.18.0.1:50031")), m("r3", R2, Some("198.18.0.1:50031"))];
        assert_eq!(verdict(tun, None, None).0, "proxy");
        assert_eq!(verdict(same(), None, Some("198.51.100.4")), v("proxy", "proxy", Some("exit_mismatch")));
        assert_eq!(verdict(same(), None, Some("203.0.113.7")).0, "punch", "网站和 STUN 一致");
        // 填的地址
        assert_eq!(verdict(same(), Some(Some("203.0.113.7")), None).0, "punch");
        assert_eq!(verdict(same(), Some(Some("117.174.0.9")), None), v("wrong_addr", "eim", Some("mismatch")));
        assert_eq!(verdict(sym(), Some(Some("117.174.0.9")), None), v("wrong_addr", "symmetric", Some("mismatch")));
        assert_eq!(verdict(same(), Some(None), None), v("wrong_addr", "eim", Some("unresolved")));
        assert_eq!(verdict(same(), Some(Some("100.101.102.103")), None).0, "punch", "Tailscale 地址不和出口比");
        // 测不了
        assert_eq!(verdict(vec![m("r1", R1, Some("203.0.113.7:50031"))], None, None), v("unknown", "skipped", Some("one_relay")));
        assert_eq!(verdict(vec![m("r1", R1, None), m("r3", R2, Some("203.0.113.7:50031"))], None, None), v("unknown", "skipped", Some("no_reply")));
        assert_eq!(verdict(Vec::new(), None, None), v("unknown", "skipped", Some("one_relay")));
        // 只有一台回了也能发现填错了地址
        assert_eq!(verdict(vec![m("r1", R1, Some("203.0.113.7:50031")), m("r3", R2, None)], Some(Some("117.174.0.9")), None).0, "wrong_addr");
    }

    /// 本机起两个 STUN 假服务（回不同的映射端口）走一遍 `stun_mapped_on`。
    #[tokio::test]
    async fn asks_two_servers_from_one_socket() {
        async fn fake(port_out: u16) -> SocketAddr {
            let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = s.local_addr().unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    let (n, from) = s.recv_from(&mut buf).await.unwrap();
                    if n < 20 {
                        continue;
                    }
                    let mut r = vec![0x01, 0x01, 0, 12];
                    r.extend_from_slice(&buf[4..20]);
                    let port = (port_out ^ 0x2112).to_be_bytes();
                    let ip = [203 ^ 0x21, 0 ^ 0x12, 113 ^ 0xa4, 7 ^ 0x42];
                    r.extend_from_slice(&[0, 0x20, 0, 8, 0, 1, port[0], port[1], ip[0], ip[1], ip[2], ip[3]]);
                    let _ = s.send_to(&r, from).await;
                }
            });
            addr
        }
        let (a, b) = (fake(50031).await, fake(61000).await);
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(crate::relay::stun_mapped_on(&sock, a).await.unwrap(), "203.0.113.7:50031".parse().unwrap());
        assert_eq!(crate::relay::stun_mapped_on(&sock, b).await.unwrap(), "203.0.113.7:61000".parse().unwrap());
    }

    /// 联网：`NAT_STUN=r1.example.com:3478,r3.example.com:3478 cargo test nat_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn nat_live() {
        let list = std::env::var("NAT_STUN").unwrap();
        let socks = bind(50000, 50031).await;
        let (sock, in_range) = (socks.b.unwrap(), socks.in_range);
        let mut ms = Vec::new();
        for (i, s) in list.split(',').enumerate() {
            let target = crate::relay::resolve(s).await.unwrap();
            let r = crate::relay::stun_mapped_on(&sock, target).await;
            ms.push(Mapping { relay: format!("r{i}"), target: Some(target), mapped: r.as_ref().ok().copied(), error: r.err() });
        }
        let r = classify(ms, None, None, Reach::skipped("unsupported"), 0, sock.local_addr().unwrap().port(), in_range);
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
    }
}
