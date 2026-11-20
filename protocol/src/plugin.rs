//! 插件协议（looklook-client/docs/BROWSER_PLUGIN_DESIGN_2026-11-19.md）：看看客户端、服务端、闭源插件三方共用。
//!
//! - **清单**（[`Manifest`]）：插件包里的 `manifest.json`，说明名字、版本、要的协议版本、怎么启动、界面入口。
//! - **发布**（[`Release`] + [`ReleaseSig`]）：服务端插件目录里每个平台一份 `release.json`（包的大小、SHA-256、清单），
//!   用离线私钥签名（`release.sig`）。客户端编译时内置公钥（`LOOKLOOK_PLUGIN_KEYS`），签名不对、哈希不对都不装。
//! - **授权凭证**（[`LicenseClaims`]）：服务端按“账号 × 插件”的授权（管理后台手动开通，带到期时间）签发的短期凭证，
//!   用平台私钥签名；插件内置平台公钥自己验，过期就不干活。看看只负责代插件向服务端申请（客户端是开源的，不靠它判权）。
//! - **本机交互**：插件监听看看给的 Unix Socket，看看把 `/p/<名>/…`（页面）和 `/api/plugins/<名>/…`（接口）转过去，
//!   每个请求带 [`HDR_TOKEN`]；插件调看看的宿主接口（[`HOST_PREFIX`]）带 [`ENV_HOST_TOKEN`] 给的口令。

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::signing::{b64, b64_decode, sha256_hex};

/// 协议版本：清单里的 `protocol` 不等于它，看看就不启动（提示更新看看或插件）
pub const PROTOCOL: u32 = 1;

/// 插件页面：`/p/<名>/…`
pub const PAGE_PREFIX: &str = "/p/";
/// 插件接口：`/api/plugins/<名>/…`
pub const API_PREFIX: &str = "/api/plugins/";
/// 看看给插件用的宿主接口（只在本机、只认插件口令）：`/_host/license`、`/_host/info`
pub const HOST_PREFIX: &str = "/_host/";
pub const HOST_LICENSE_PATH: &str = "/_host/license";
pub const HOST_INFO_PATH: &str = "/_host/info";

/// 看看启动插件时给的环境变量
pub const ENV_SOCKET: &str = "LOOKLOOK_PLUGIN_SOCKET";
pub const ENV_TOKEN: &str = "LOOKLOOK_PLUGIN_TOKEN";
pub const ENV_DATA_DIR: &str = "LOOKLOOK_PLUGIN_DATA_DIR";
pub const ENV_BASE: &str = "LOOKLOOK_PLUGIN_BASE";
pub const ENV_HOST_URL: &str = "LOOKLOOK_HOST_URL";
pub const ENV_HOST_TOKEN: &str = "LOOKLOOK_HOST_TOKEN";
pub const ENV_CLIENT_VERSION: &str = "LOOKLOOK_CLIENT_VERSION";

/// 看看转给插件的请求带的头：口令（插件只认带对口令的请求）、是不是这台电脑自己打开的（`1` / `0`）
pub const HDR_TOKEN: &str = "x-ll-plugin-token";
pub const HDR_LOCAL: &str = "x-ll-local";

/// 插件名：小写字母、数字、`-`，2～32 个字符，字母开头
pub fn valid_name(name: &str) -> bool {
    (2..=32).contains(&name.len()) && name.starts_with(|c: char| c.is_ascii_lowercase()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 版本号：`1.2.3`（可带 `-beta.1` 这样的后缀）
pub fn valid_version(v: &str) -> bool {
    let core = v.split('-').next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    v.len() <= 40 && parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 6 && p.chars().all(|c| c.is_ascii_digit())) && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c))
}

/// 比较版本号（只比 `主.次.修订`，后缀不管）
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let nums = |v: &str| -> Vec<u64> { v.split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    nums(a).cmp(&nums(b))
}

/// 路径属于哪个插件：`/p/<名>/…` 或 `/api/plugins/<名>/…`
pub fn plugin_of_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(PAGE_PREFIX).or_else(|| path.strip_prefix(API_PREFIX))?;
    let name = rest.split('/').next()?;
    valid_name(name).then_some(name)
}

/// 这台电脑的平台：`linux-x86_64`、`linux-aarch64`、`macos-aarch64`、`windows-x86_64`……
pub fn current_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

pub fn valid_platform(p: &str) -> bool {
    (3..=40).contains(&p.len()) && p.contains('-') && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// 插件包里的 `manifest.json`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// 要的协议版本（[`PROTOCOL`]）
    pub protocol: u32,
    /// 要求的最低看看版本
    #[serde(default)]
    pub min_client: String,
    /// 显示名、说明：语言 → 文字（`zh-CN`、`en-US`）
    pub title: BTreeMap<String, String>,
    #[serde(default)]
    pub description: BTreeMap<String, String>,
    /// 图标：lucide 图标名（`globe`、`puzzle`……），界面认不出时用拼图
    #[serde(default)]
    pub icon: String,
    /// 包里的可执行文件（相对包根目录），例如 `bin/looklook-browser`
    pub exec: String,
    /// 看看启动时如果插件数据目录下有这个文件（相对路径），就先把插件起来（例如浏览器还在后台跑，要接回来）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart_if_exists: Option<String>,
    /// 插件能打开网址（对话里点链接时菜单里多一项“用 <插件> 打开”）：`POST /api/plugins/<名>/open-url {url}`
    #[serde(default)]
    pub open_url: bool,
}

impl Manifest {
    /// 检查字段（看看装之前、服务端打包时都查）
    pub fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err(format!("bad name {:?}", self.name));
        }
        if !valid_version(&self.version) {
            return Err(format!("bad version {:?}", self.version));
        }
        if !self.min_client.is_empty() && !valid_version(&self.min_client) {
            return Err(format!("bad min_client {:?}", self.min_client));
        }
        if self.title.values().all(|t| t.trim().is_empty()) {
            return Err("missing title".into());
        }
        let exec = self.exec.as_str();
        let safe = !exec.is_empty() && !exec.starts_with('/') && !exec.contains('\\') && exec.split('/').all(|p| !p.is_empty() && p != "." && p != "..");
        if !safe {
            return Err(format!("bad exec {:?}", self.exec));
        }
        if let Some(f) = &self.autostart_if_exists {
            if f.is_empty() || f.starts_with('/') || f.split('/').any(|p| p.is_empty() || p == "..") {
                return Err(format!("bad autostart_if_exists {f:?}"));
            }
        }
        Ok(())
    }

    /// 按界面语言挑显示名（没有就用英文，再没有用随便哪个）
    pub fn title_for(&self, lang: &str) -> String {
        pick_lang(&self.title, lang).unwrap_or_else(|| self.name.clone())
    }
}

pub fn pick_lang(m: &BTreeMap<String, String>, lang: &str) -> Option<String> {
    m.get(lang).or_else(|| m.get("en-US")).or_else(|| m.values().next()).filter(|s| !s.trim().is_empty()).cloned()
}

/// 服务端插件目录里一个平台的发布：`<plugins_dir>/<名>/<平台>/release.json`（旁边是 `release.sig` 和 `package.tar.gz`）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub name: String,
    pub version: String,
    pub platform: String,
    /// `package.tar.gz` 的 SHA-256（hex）和字节数
    pub sha256: String,
    pub size: u64,
    pub manifest: Manifest,
    /// 打包时间（RFC 3339）
    #[serde(default)]
    pub published_at: String,
}

impl Release {
    pub fn validate(&self) -> Result<(), String> {
        self.manifest.validate()?;
        if self.manifest.name != self.name || self.manifest.version != self.version {
            return Err("manifest does not match release".into());
        }
        if !valid_platform(&self.platform) {
            return Err(format!("bad platform {:?}", self.platform));
        }
        if self.sha256.len() != 64 || !self.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("bad sha256".into());
        }
        Ok(())
    }
}

/// `release.sig`：离线私钥对 `release.json` 原文的签名
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSig {
    /// 用的哪把公钥（`LOOKLOOK_PLUGIN_KEYS` 里的名字，例如 `pk1`）
    pub kid: String,
    pub sig: String,
}

fn release_message(release_json: &[u8]) -> String {
    format!("LL1-PLUGIN-RELEASE\n{}", sha256_hex(release_json))
}

pub fn sign_release(key: &SigningKey, kid: &str, release_json: &[u8]) -> ReleaseSig {
    ReleaseSig { kid: kid.to_string(), sig: b64(&key.sign(release_message(release_json).as_bytes()).to_bytes()) }
}

/// 验 `release.json` 原文的签名，通过了才解析
pub fn verify_release(keys: &[(String, VerifyingKey)], release_json: &[u8], sig: &ReleaseSig) -> Result<Release, String> {
    let key = keys.iter().find(|(k, _)| *k == sig.kid).map(|(_, k)| k).ok_or_else(|| format!("unknown key {:?}", sig.kid))?;
    if !verify_raw(key, release_message(release_json).as_bytes(), &sig.sig) {
        return Err("bad signature".into());
    }
    let r: Release = serde_json::from_slice(release_json).map_err(|e| format!("release.json: {e}"))?;
    r.validate()?;
    Ok(r)
}

fn verify_raw(key: &VerifyingKey, msg: &[u8], sig_b64: &str) -> bool {
    let Some(bytes) = b64_decode(sig_b64) else { return false };
    let Ok(bytes): Result<[u8; 64], _> = bytes.try_into() else { return false };
    key.verify(msg, &Signature::from_bytes(&bytes)).is_ok()
}

/// 公钥列表：`pk1=<base64url>,pk2=<base64url>`（和 `LOOKLOOK_PLATFORM_KEYS` 一个写法）。写错的跳过
pub fn parse_keys(s: &str) -> Vec<(String, VerifyingKey)> {
    s.split([',', ';', '\n'])
        .filter_map(|part| {
            let (kid, key) = part.trim().split_once('=')?;
            let bytes: [u8; 32] = b64_decode(key.trim())?.try_into().ok()?;
            Some((kid.trim().to_string(), VerifyingKey::from_bytes(&bytes).ok()?))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

// ---------------- 授权凭证 ----------------

/// 授权凭证里的内容
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LicenseClaims {
    pub v: u32,
    pub plugin: String,
    /// 账号 id
    pub uid: i64,
    /// 设备 public id
    pub did: String,
    /// 插件报的机器指纹（哈希过的），凭证只在这台机器上有效
    pub mach: String,
    /// 签发、到期时间（Unix 秒）
    pub iat: i64,
    pub exp: i64,
    /// 管理后台给的授权什么时候到期（界面上显示）
    pub ent_exp: i64,
    /// 水印：每次签发不同，泄露了能查到是谁
    pub wm: String,
}

const LICENSE_TAG: &str = "LL1L";

fn license_message(kid: &str, payload: &str) -> String {
    format!("LL1-PLUGIN-LICENSE\n{kid}\n{payload}")
}

/// 签发：`LL1L.<kid>.<内容 base64url>.<签名 base64url>`
pub fn issue_license(key: &SigningKey, kid: &str, claims: &LicenseClaims) -> String {
    let payload = b64(&serde_json::to_vec(claims).expect("claims"));
    let sig = b64(&key.sign(license_message(kid, &payload).as_bytes()).to_bytes());
    format!("{LICENSE_TAG}.{kid}.{payload}.{sig}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseError {
    Malformed,
    UnknownKey,
    BadSignature,
    WrongPlugin,
    WrongMachine,
    Expired,
}

/// 插件验凭证：签名、插件名、机器、有没有过期（`now` 是 Unix 秒）
pub fn verify_license(keys: &[(String, VerifyingKey)], token: &str, plugin: &str, machine: &str, now: i64) -> Result<LicenseClaims, LicenseError> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    let [tag, kid, payload, sig] = parts.as_slice() else { return Err(LicenseError::Malformed) };
    if *tag != LICENSE_TAG {
        return Err(LicenseError::Malformed);
    }
    let key = keys.iter().find(|(k, _)| k == kid).map(|(_, k)| k).ok_or(LicenseError::UnknownKey)?;
    if !verify_raw(key, license_message(kid, payload).as_bytes(), sig) {
        return Err(LicenseError::BadSignature);
    }
    let claims: LicenseClaims = b64_decode(payload).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(LicenseError::Malformed)?;
    if claims.plugin != plugin {
        return Err(LicenseError::WrongPlugin);
    }
    if claims.mach != machine {
        return Err(LicenseError::WrongMachine);
    }
    if now >= claims.exp {
        return Err(LicenseError::Expired);
    }
    Ok(claims)
}

/// 凭证最长多久（之后联网续期）
pub const LICENSE_TTL_SECS: i64 = 7 * 24 * 3600;
/// 机器指纹：插件把自己算的原始指纹哈希一下再交出去（不把 machine-id 原样传给服务端）
pub fn machine_digest(raw: &str) -> String {
    sha256_hex(format!("looklook-plugin-machine\n{raw}").as_bytes())[..32].to_string()
}

// ---------------- 客户端 ↔ 服务端 ----------------

/// `GET /api/client/v1/plugins?platform=…`：这个平台能装的插件，和这个账号的授权
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogItem {
    pub name: String,
    pub version: String,
    pub platform: String,
    pub size: u64,
    pub title: BTreeMap<String, String>,
    #[serde(default)]
    pub description: BTreeMap<String, String>,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub min_client: String,
    /// 这个账号有没有有效授权、授权到什么时候（RFC 3339）
    pub licensed: bool,
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub items: Vec<CatalogItem>,
}

/// `GET /api/client/v1/plugins/{name}/release?platform=…`：签过名的 `release.json` 原文（base64url）和签名
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedRelease {
    pub release: String,
    pub sig: ReleaseSig,
}

/// `POST /api/client/v1/plugins/{name}/license`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LicenseRequest {
    pub machine: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LicenseResponse {
    pub token: String,
    /// 凭证到期（Unix 秒）、授权到期（RFC 3339）
    pub exp: i64,
    pub expires_at: String,
}

pub fn catalog_path() -> &'static str {
    "/api/client/v1/plugins"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            name: "browser".into(),
            version: "1.2.0".into(),
            protocol: PROTOCOL,
            min_client: "1.88.0".into(),
            title: BTreeMap::from([("zh-CN".into(), "远程浏览器".into()), ("en-US".into(), "Remote browser".into())]),
            description: BTreeMap::new(),
            icon: "globe".into(),
            exec: "bin/looklook-browser".into(),
            autostart_if_exists: Some("run.json".into()),
            open_url: true,
        }
    }

    #[test]
    fn names_versions_paths() {
        assert!(valid_name("browser") && valid_name("my-tool2"));
        assert!(!valid_name("B") && !valid_name("1abc") && !valid_name("a/b") && !valid_name(&"a".repeat(33)));
        assert!(valid_version("1.0.12") && valid_version("2.0.0-beta.1"));
        assert!(!valid_version("1.0") && !valid_version("1.0.x") && !valid_version("1..0"));
        assert_eq!(version_cmp("1.10.0", "1.9.9"), std::cmp::Ordering::Greater);
        assert_eq!(version_cmp("1.2.3-rc.1", "1.2.3"), std::cmp::Ordering::Equal);
        assert_eq!(plugin_of_path("/p/browser/view/"), Some("browser"));
        assert_eq!(plugin_of_path("/api/plugins/browser/status"), Some("browser"));
        assert_eq!(plugin_of_path("/p/../x"), None);
        assert_eq!(plugin_of_path("/api/instances/x"), None);
        assert!(valid_platform(&current_platform()));
    }

    #[test]
    fn manifest_checks() {
        let m = manifest();
        assert!(m.validate().is_ok());
        assert_eq!(m.title_for("zh-CN"), "远程浏览器");
        assert_eq!(m.title_for("ja"), "Remote browser");
        for bad in ["/usr/bin/x", "../x", "bin/../../x", "", "bin\\x", "./x"] {
            assert!(Manifest { exec: bad.into(), ..manifest() }.validate().is_err(), "{bad}");
        }
        assert!(Manifest { autostart_if_exists: Some("../run.json".into()), ..manifest() }.validate().is_err());
        assert!(Manifest { title: BTreeMap::new(), ..manifest() }.validate().is_err());
    }

    #[test]
    fn release_signature() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let keys = parse_keys(&format!("pk1={}, junk, pk2=notakey", b64(sk.verifying_key().as_bytes())));
        assert_eq!(keys.len(), 1);
        let r = Release { name: "browser".into(), version: "1.2.0".into(), platform: "linux-x86_64".into(), sha256: "a".repeat(64), size: 10, manifest: manifest(), published_at: String::new() };
        let raw = serde_json::to_vec(&r).unwrap();
        let sig = sign_release(&sk, "pk1", &raw);
        assert_eq!(verify_release(&keys, &raw, &sig).unwrap(), r);
        let mut tampered = raw.clone();
        tampered[10] ^= 1;
        assert!(verify_release(&keys, &tampered, &sig).is_err());
        assert!(verify_release(&keys, &raw, &ReleaseSig { kid: "pk9".into(), ..sig.clone() }).is_err());
        let other = SigningKey::from_bytes(&[8u8; 32]);
        assert!(verify_release(&keys, &raw, &sign_release(&other, "pk1", &raw)).is_err());
        // 清单和发布对不上
        let bad = Release { version: "9.9.9".into(), ..r };
        let raw = serde_json::to_vec(&bad).unwrap();
        assert!(verify_release(&keys, &raw, &sign_release(&sk, "pk1", &raw)).is_err());
    }

    #[test]
    fn license_roundtrip() {
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let keys = vec![("p1".to_string(), sk.verifying_key())];
        let mach = machine_digest("machine-id-1");
        let c = LicenseClaims { v: 1, plugin: "browser".into(), uid: 5, did: "dev1".into(), mach: mach.clone(), iat: 100, exp: 200, ent_exp: 300, wm: "w".into() };
        let t = issue_license(&sk, "p1", &c);
        assert_eq!(verify_license(&keys, &t, "browser", &mach, 150).unwrap(), c);
        assert_eq!(verify_license(&keys, &t, "browser", &mach, 200), Err(LicenseError::Expired));
        assert_eq!(verify_license(&keys, &t, "other", &mach, 150), Err(LicenseError::WrongPlugin));
        assert_eq!(verify_license(&keys, &t, "browser", "x", 150), Err(LicenseError::WrongMachine));
        assert_eq!(verify_license(&[], &t, "browser", &mach, 150), Err(LicenseError::UnknownKey));
        assert_eq!(verify_license(&keys, "nope", "browser", &mach, 150), Err(LicenseError::Malformed));
        // 改了内容（比如把到期时间往后挪）签名就不对了
        let mut parts: Vec<String> = t.split('.').map(String::from).collect();
        parts[2] = b64(&serde_json::to_vec(&LicenseClaims { exp: 9_999_999_999, ..c }).unwrap());
        assert_eq!(verify_license(&keys, &parts.join("."), "browser", &mach, 150), Err(LicenseError::BadSignature));
    }
}
