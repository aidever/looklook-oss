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
    /// 插件接的钩子（[`HOOK_FEATURES`]、[`HOOK_FEISHU_GROUP`]……）：看看到时候调 `POST /_hooks/<钩子>`（[`hook_path`]）。
    /// 老看看不认这个字段，照常装、照常转页面和接口，只是不调钩子
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<String>,
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
        if self.hooks.len() > 16 || self.hooks.iter().any(|h| !valid_name(h)) {
            return Err(format!("bad hooks {:?}", self.hooks));
        }
        Ok(())
    }

    pub fn has_hook(&self, hook: &str) -> bool {
        self.hooks.iter().any(|h| h == hook)
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

// ---------------- 机器指纹（看看和所有插件共用一套算法） ----------------

/// 安装号文件：放在看看数据目录（`LOOKLOOK_HOME`）里，看看和插件读同一个，算出来的指纹才一样
pub const MACHINE_INSTALL_FILE: &str = "machine-install-id";
/// 看看数据目录（插件拿它找安装号）
pub const ENV_HOME: &str = "LOOKLOOK_HOME";

/// 系统自己的机器号：Linux `/etc/machine-id`，macOS `IOPlatformUUID`，Windows `MachineGuid`。没有时为空
/// （例如有的容器），这时只靠安装号区分机器
pub fn os_machine_id() -> String {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/sbin/ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output();
        if let Ok(o) = out {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Some(v) = text.lines().find(|l| l.contains("\"IOPlatformUUID\"")).and_then(|l| l.split('"').nth(3)) {
                return v.trim().to_string();
            }
        }
        String::new()
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("reg").args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"]).output();
        if let Ok(o) = out {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Some(v) = text.lines().find(|l| l.contains("MachineGuid")).and_then(|l| l.split_whitespace().last()) {
                return v.trim().to_string();
            }
        }
        String::new()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        ["/etc/machine-id", "/var/lib/dbus/machine-id"].iter().find_map(|p| std::fs::read_to_string(p).ok()).map(|s| s.trim().to_string()).unwrap_or_default()
    }
}

fn valid_install_id(s: &str) -> bool {
    (16..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 读安装号，没有就生成一个（只用来区分机器，不是秘密）。两个进程同时生成时以先写下的为准
pub fn install_id(home: &std::path::Path) -> std::io::Result<String> {
    let path = home.join(MACHINE_INSTALL_FILE);
    if let Ok(s) = std::fs::read_to_string(&path) {
        if valid_install_id(s.trim()) {
            return Ok(s.trim().to_string());
        }
        let _ = std::fs::remove_file(&path);
    }
    std::fs::create_dir_all(home)?;
    let seed = format!(
        "{:?}|{}|{}|{:p}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default(),
        std::process::id(),
        home.display(),
        &path
    );
    let fresh = sha256_hex(seed.as_bytes())[..26].to_string();
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            f.write_all(fresh.as_bytes())?;
            Ok(fresh)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let s = std::fs::read_to_string(&path)?;
            if valid_install_id(s.trim()) {
                Ok(s.trim().to_string())
            } else {
                Err(std::io::Error::other("bad install id"))
            }
        }
        Err(e) => Err(e),
    }
}

/// 这台机器的指纹（32 位 hex）：系统机器号 + 看看数据目录里的安装号，再哈希。
/// 看看在插件页显示它（离线授权就按它签），插件自己也按同一个算法算、拿来核对凭证
pub fn machine_fingerprint(home: &std::path::Path) -> std::io::Result<String> {
    Ok(fingerprint_of(&os_machine_id(), &install_id(home)?))
}

pub fn fingerprint_of(os_id: &str, install_id: &str) -> String {
    machine_digest(&format!("{os_id}|{install_id}"))
}

pub fn valid_machine(m: &str) -> bool {
    m.len() == 32 && m.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// 用户粘贴的指纹：去掉空格、横线，转小写
pub fn normalize_machine(s: &str) -> Option<String> {
    let m: String = s.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_lowercase();
    valid_machine(&m).then_some(m)
}

// ---------------- 离线授权码 ----------------

/// 离线授权的 `did`（没有账号、设备，`uid` 为 0）
pub const OFFLINE_DID: &str = "offline";

/// 离线授权码：管理后台按机器指纹给一个或几个插件签的凭证（和联网领的同一个格式，只是没有 7 天上限，到期为止），
/// 几个凭证之间用空格隔开。这里只拆开、读出内容，不验签
pub fn split_offline_code(code: &str) -> Vec<String> {
    code.split(|c: char| c.is_whitespace() || c == ',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

/// 只读凭证里的内容（不验签，看看用来显示和挑出插件名；真的判权在插件里）
pub fn peek_license(token: &str) -> Option<LicenseClaims> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    let [tag, _kid, payload, _sig] = parts.as_slice() else { return None };
    if *tag != LICENSE_TAG {
        return None;
    }
    b64_decode(payload).and_then(|b| serde_json::from_slice(&b).ok())
}

// ---------------- 离线安装包 ----------------

/// 离线安装包（`.llplugin`）：在联网的电脑上下载，拷到别的电脑上导入。
/// 格式：`LLPLUGIN1\n` + 8 字节大端长度 + `release.json` 原文 + 8 字节大端长度 + `release.sig` + 后面到结尾是 `package.tar.gz`。
/// 导入时照样验 `release.json` 的签名和包的 SHA-256
pub const OFFLINE_PACKAGE_MAGIC: &[u8] = b"LLPLUGIN1\n";
/// 离线安装包的扩展名
pub const OFFLINE_PACKAGE_EXT: &str = "llplugin";
const OFFLINE_HEAD_MAX: u64 = 1 << 20;

/// 离线安装包的开头（后面直接接 `package.tar.gz`）
pub fn offline_package_head(release_json: &[u8], sig: &ReleaseSig) -> Vec<u8> {
    let sig = serde_json::to_vec(sig).expect("sig");
    let mut out = Vec::with_capacity(OFFLINE_PACKAGE_MAGIC.len() + 16 + release_json.len() + sig.len());
    out.extend_from_slice(OFFLINE_PACKAGE_MAGIC);
    out.extend_from_slice(&(release_json.len() as u64).to_be_bytes());
    out.extend_from_slice(release_json);
    out.extend_from_slice(&(sig.len() as u64).to_be_bytes());
    out.extend_from_slice(&sig);
    out
}

/// 读离线安装包的开头：`release.json` 原文和签名；读完后 `r` 停在包开始的地方
pub fn read_offline_package_head(r: &mut impl std::io::Read) -> Result<(Vec<u8>, ReleaseSig), String> {
    let mut magic = vec![0u8; OFFLINE_PACKAGE_MAGIC.len()];
    r.read_exact(&mut magic).map_err(|_| "not a plugin package".to_string())?;
    if magic != OFFLINE_PACKAGE_MAGIC {
        return Err("not a plugin package".into());
    }
    let chunk = |r: &mut dyn std::io::Read| -> Result<Vec<u8>, String> {
        let mut n = [0u8; 8];
        r.read_exact(&mut n).map_err(|_| "truncated".to_string())?;
        let n = u64::from_be_bytes(n);
        if n > OFFLINE_HEAD_MAX {
            return Err("bad header".into());
        }
        let mut b = vec![0u8; n as usize];
        r.read_exact(&mut b).map_err(|_| "truncated".to_string())?;
        Ok(b)
    };
    let raw = chunk(r)?;
    let sig = chunk(r)?;
    let sig: ReleaseSig = serde_json::from_slice(&sig).map_err(|_| "bad signature".to_string())?;
    Ok((raw, sig))
}

// ---------------- 钩子（看看 → 插件） ----------------
//
// 看看在某些时候问插件要主意：经插件的 Socket 发 `POST /_hooks/<钩子>`（带 [`HDR_TOKEN`]），请求体、回答都是 JSON。
// 插件只出主意，真正动手的（开只读会话、发飞书）仍由看看做，安全上的限制（只读闸门、并发、超时）也由看看管，插件放不开。
// 插件出错、超时、回了认不出的东西时，看看按“不做”处理。

pub const HOOK_PREFIX: &str = "/_hooks/";
/// `{}` → [`Features`]：插件现在开着的功能（没授权、过期时不列）
pub const HOOK_FEATURES: &str = "features";
/// [`GroupMessage`] → [`GroupDecision`]：飞书群里不是主人的人 @ 了机器人，看看没别的安排（不是在等的人）时问插件
pub const HOOK_FEISHU_GROUP: &str = "feishu-group";
/// [`AskDone`] → `{}`：按插件的主意开的只读提问答完了（记录、统计用）
pub const HOOK_ASK_DONE: &str = "ask-done";

/// 功能名：新建会话记下操作人署名（`created_by`），会话列表能按人看
pub const FEATURE_OPERATOR: &str = "operator";
/// 功能名：飞书群成员 @ 机器人提问（只读）
pub const FEATURE_GROUP_ASK: &str = "group_ask";

/// 只读提问最长跑多久（秒）：插件给的超过就按这个
pub const ASK_TIMEOUT_MAX: u64 = 600;
pub const ASK_TIMEOUT_DEFAULT: u64 = 180;
/// 问题最长（字符），超过的截掉
pub const ASK_QUESTION_MAX: usize = 4000;

pub fn hook_path(hook: &str) -> String {
    format!("{HOOK_PREFIX}{hook}")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Features {
    #[serde(default)]
    pub features: Vec<String>,
}

/// 飞书群里的一条 @ 机器人的消息（@ 已经去掉、换成名字）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupMessage {
    pub chat_id: String,
    pub message_id: String,
    /// 发消息的人（open_id）和看看认得的名字（可能为空）
    pub sender: String,
    #[serde(default)]
    pub sender_name: String,
    pub text: String,
    /// 带了图片、文件（只读提问看不到）
    #[serde(default)]
    pub attachments: bool,
}

/// 插件的主意。认不出的一律当 [`GroupDecision::Ignore`]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GroupDecision {
    /// 不管（看看照老样子转给主人）
    Ignore,
    /// 在群里回这句并 @ 他（比如“今天问得太多了”），不开会话
    Reply { text: String },
    /// 开一个一次性的只读会话回答 `question`，答案回到群里 @ 他
    Ask {
        question: String,
        /// 只读会话的工作目录（这台电脑上的绝对路径）；空、不存在时用看看给的空目录
        #[serde(default)]
        workdir: String,
        /// 最长跑多久（秒），0 = [`ASK_TIMEOUT_DEFAULT`]，最多 [`ASK_TIMEOUT_MAX`]
        #[serde(default)]
        timeout_secs: u64,
        /// 插件自己的编号，[`AskDone`] 里带回来
        #[serde(default)]
        ask_id: String,
    },
}

impl GroupDecision {
    /// 解析插件的回答：认不出就是不管
    pub fn parse(v: &serde_json::Value) -> GroupDecision {
        serde_json::from_value(v.clone()).unwrap_or(GroupDecision::Ignore)
    }
}

/// 只读提问的结果
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskDone {
    #[serde(default)]
    pub ask_id: String,
    pub chat_id: String,
    pub sender: String,
    /// `answered`、`failed`、`timeout`、`busy`（同时在答的太多）、`unavailable`（这台电脑开不了只读会话）
    pub outcome: String,
    #[serde(default)]
    pub secs: u64,
    #[serde(default)]
    pub answer_chars: u64,
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
            hooks: vec![],
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
    fn hooks_and_decisions() {
        let m = Manifest { hooks: vec![HOOK_FEATURES.into(), HOOK_FEISHU_GROUP.into()], ..manifest() };
        assert!(m.validate().is_ok() && m.has_hook(HOOK_FEISHU_GROUP) && !m.has_hook(HOOK_ASK_DONE));
        assert!(Manifest { hooks: vec!["../x".into()], ..manifest() }.validate().is_err());
        // 老清单没有 hooks：照常解析；没有 hooks 时也不写出来
        let old: Manifest = serde_json::from_value(serde_json::json!({ "name": "browser", "version": "1.0.0", "protocol": 1, "title": { "en-US": "B" }, "exec": "bin/b" })).unwrap();
        assert!(old.hooks.is_empty() && !serde_json::to_string(&old).unwrap().contains("hooks"));
        assert_eq!(hook_path(HOOK_FEISHU_GROUP), "/_hooks/feishu-group");
        let ask = GroupDecision::parse(&serde_json::json!({ "action": "ask", "question": "接口在哪", "ask_id": "a1" }));
        assert_eq!(ask, GroupDecision::Ask { question: "接口在哪".into(), workdir: String::new(), timeout_secs: 0, ask_id: "a1".into() });
        assert_eq!(GroupDecision::parse(&serde_json::json!({ "action": "reply", "text": "太多了" })), GroupDecision::Reply { text: "太多了".into() });
        for bad in [serde_json::json!({ "action": "run_shell" }), serde_json::json!(null), serde_json::json!({ "action": "ask" })] {
            assert_eq!(GroupDecision::parse(&bad), GroupDecision::Ignore, "{bad}");
        }
    }

    #[test]
    fn fingerprint_and_offline_code() {
        let d = std::env::temp_dir().join(format!("llp-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let a = machine_fingerprint(&d).unwrap();
        // 安装号存下来了：再算一样
        assert_eq!(machine_fingerprint(&d).unwrap(), a);
        assert!(valid_machine(&a));
        assert_ne!(fingerprint_of("x", &install_id(&d).unwrap()), fingerprint_of("y", &install_id(&d).unwrap()));
        // 坏掉的安装号重新生成
        std::fs::write(d.join(MACHINE_INSTALL_FILE), "../bad").unwrap();
        assert_ne!(machine_fingerprint(&d).unwrap(), a);
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(normalize_machine(&format!(" {} ", a.to_uppercase())), Some(a.clone()));
        assert_eq!(normalize_machine("abcd-"), None);

        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let c = |p: &str| LicenseClaims { v: 1, plugin: p.into(), uid: 0, did: OFFLINE_DID.into(), mach: a.clone(), iat: 1, exp: 99, ent_exp: 99, wm: "w".into() };
        let code = format!("{}  {}\n", issue_license(&sk, "p1", &c("team")), issue_license(&sk, "p1", &c("browser")));
        let parts = split_offline_code(&code);
        assert_eq!(parts.len(), 2);
        assert_eq!(peek_license(&parts[1]).unwrap().plugin, "browser");
        assert!(peek_license("LL1L.x.y").is_none());
    }

    #[test]
    fn offline_package() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let raw = br#"{"x":1}"#.to_vec();
        let sig = sign_release(&sk, "pk1", &raw);
        let mut file = offline_package_head(&raw, &sig);
        file.extend_from_slice(b"PACKAGE");
        let mut r = std::io::Cursor::new(file);
        let (r2, s2) = read_offline_package_head(&mut r).unwrap();
        assert_eq!((r2, s2), (raw, sig));
        let mut rest = vec![];
        std::io::Read::read_to_end(&mut r, &mut rest).unwrap();
        assert_eq!(rest, b"PACKAGE");
        assert!(read_offline_package_head(&mut std::io::Cursor::new(b"PK\x03\x04junk".to_vec())).is_err());
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
