//! 插件协议（客户端 docs/BROWSER_PLUGIN_DESIGN_2026-11-19.md、docs/PLUGIN_PROTOCOL.md）。
//!
//! 看看客户端只做通用的插件管理（安装、验签、启动、转发、菜单入口），不认识任何具体插件；
//! 插件是单独的程序，只经这里定义的 HTTP + JSON 协议和看看交互。三方：
//!
//! - **看看 → 插件**：插件监听看看给的 Unix socket（[`ENV_SOCKET`]），看看把 `/api/plugins/<名>/…`、`/p/<名>/…`
//!   原样（不去前缀）转给它，带上 [`HDR_TOKEN`] 和访问来源头；另有固定的 [`PLUGIN_STATUS_PATH`] 等。
//! - **插件 → 看看**：只给白名单接口（[`HOST_INFO_PATH`] 等），走 [`ENV_HOST_URL`]，同样带 [`HDR_TOKEN`]。
//! - **看看 ↔ 平台**：插件目录、下载、授权凭证、按凭证下发的数据（[`CLIENT_CATALOG_PATH`] 等，设备签名）。
//!
//! 插件包：tar.gz，顶层是 [`MANIFEST_FILE`]、[`PACKAGE_FILE`]（全部文件的 SHA-256）、[`SIGNATURE_FILE`]
//! （离线私钥对 [`PACKAGE_FILE`] 的签名）和其他文件。授权凭证由平台签名（与平台响应同一把钥匙），插件内置平台公钥自己验。

use std::collections::{BTreeMap, HashMap};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::signing::{b64, b64_decode, sha256_hex, sign, verify};

/// 协议版本。清单写自己支持的范围（[`ProtocolRange`]），看看不在范围内就不启动并提示更新。
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------- 包

pub const MANIFEST_FILE: &str = "manifest.json";
pub const PACKAGE_FILE: &str = "package.json";
pub const SIGNATURE_FILE: &str = "package.sig";
/// 安装包（tar.gz）的大小上限。
pub const MAX_PACKAGE_BYTES: u64 = 512 << 20;

// ---------------------------------------------------------------- 进程环境变量（看看 → 插件）

/// 插件要监听的 Unix socket 路径（目录已建好、0700）。
pub const ENV_SOCKET: &str = "LOOKLOOK_PLUGIN_SOCKET";
/// 本次启动的随机令牌：看看发来的每个请求都带 [`HDR_TOKEN`]，插件调宿主接口也带它。
pub const ENV_TOKEN: &str = "LOOKLOOK_PLUGIN_TOKEN";
/// 插件自己的持久数据目录 `<数据目录>/plugin-data/<名>/`（0700，更新插件不动它）。
pub const ENV_DATA_DIR: &str = "LOOKLOOK_PLUGIN_DATA_DIR";
/// 宿主接口的地址（`http://127.0.0.1:端口`，不带结尾 `/`）。
pub const ENV_HOST_URL: &str = "LOOKLOOK_PLUGIN_HOST_URL";
/// 插件名。
pub const ENV_NAME: &str = "LOOKLOOK_PLUGIN_NAME";
/// 看看实现的协议版本。
pub const ENV_PROTOCOL: &str = "LOOKLOOK_PLUGIN_PROTOCOL";
/// 看看的版本号。
pub const ENV_CLIENT_VERSION: &str = "LOOKLOOK_CLIENT_VERSION";
/// 看看的数据目录（只读用途：迁移旧版内置功能留下的数据，如 `<数据目录>/browser/`）。
pub const ENV_HOME: &str = "LOOKLOOK_HOME_DIR";

// ---------------------------------------------------------------- 请求头

/// 看看 ↔ 插件双向请求都带的令牌（[`ENV_TOKEN`]）。
pub const HDR_TOKEN: &str = "x-ll-plugin-token";
/// 用户从哪里打开：[`Access`] 的取值。
pub const HDR_ACCESS: &str = "x-ll-plugin-access";
/// `1`：只读访问（只读分享），插件不能执行会改变状态的操作。
pub const HDR_READONLY: &str = "x-ll-plugin-readonly";
/// 界面语言（`zh-CN` / `en-US`），插件按它返回文案（如 open_url 的目标名称）。
pub const HDR_LOCALE: &str = "x-ll-plugin-locale";

/// 请求从哪里来（转发时由看看填 [`HDR_ACCESS`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// 这台电脑自己
    Local,
    /// 局域网 / 白名单
    Lan,
    /// 远程（专属地址经中转）
    Remote,
    /// 远程（WebRTC）
    Rtc,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Local => "local",
            Access::Lan => "lan",
            Access::Remote => "remote",
            Access::Rtc => "rtc",
        }
    }
}

// ---------------------------------------------------------------- 路径

/// 看看上的插件接口前缀：`/api/plugins/<名>/…` 原样转给插件。
pub const API_PREFIX: &str = "/api/plugins/";
/// 看看上的插件页面前缀：`/p/<名>/…` 原样转给插件。
pub const PAGE_PREFIX: &str = "/p/";

/// 插件固定提供：状态（[`PluginStatus`]）。看看只在插件进程已在跑时才问，不会为它拉起插件。
pub const PLUGIN_STATUS_PATH: &str = "/_plugin/status";
/// 插件固定提供：看看要停掉它之前先发这个（POST），插件收尾后自己退出；5 秒内没退出看看再结束进程。
pub const PLUGIN_SHUTDOWN_PATH: &str = "/_plugin/shutdown";

/// 宿主接口前缀（插件 → 看看，只接受这台机器自己的连接且带 [`HDR_TOKEN`]）。
pub const HOST_PREFIX: &str = "/_host/";
/// GET → [`HostInfo`]
pub const HOST_INFO_PATH: &str = "/_host/info";
/// POST [`HostNotify`] → 204：在看看里弹一条提醒（也会照用户设置推送）。
pub const HOST_NOTIFY_PATH: &str = "/_host/notify";
/// POST [`LicenseRequest`] → [`SignedLicense`]：看看用设备私钥签名，代插件向平台申请 / 续期授权凭证。
pub const HOST_LICENSE_PATH: &str = "/_host/license";
/// GET → [`PluginData`]：看看代插件取平台按凭证下发的数据（如设备池），插件用凭证里的 `data_key` 解密。
pub const HOST_DATA_PATH: &str = "/_host/data";

// 平台接口（客户端接口前缀 `/api/client/v1` 之后的部分）

/// 不需要登录：插件目录（`?target=linux-x86_64`）→ [`PluginCatalog`]，`licensed` 全为 false。
pub const CLIENT_CATALOG_PUBLIC_PATH: &str = "/plugins/catalog";
/// 设备签名：插件目录（`?target=`）→ [`PluginCatalog`]，带本账户的授权情况。
pub const CLIENT_CATALOG_PATH: &str = "/plugins";

/// 设备签名：`POST /plugins/{name}/download`（[`DownloadRequest`]）→ [`DownloadTicket`]。
pub fn client_download_path(name: &str) -> String {
    format!("/plugins/{name}/download")
}
/// 设备签名：`POST /plugins/{name}/license`（[`LicenseRequest`]）→ [`SignedLicense`]。
pub fn client_license_path(name: &str) -> String {
    format!("/plugins/{name}/license")
}
/// 设备签名：`GET /plugins/{name}/data` → [`PluginData`]。
pub fn client_data_path(name: &str) -> String {
    format!("/plugins/{name}/data")
}

/// 插件名：小写字母开头，小写字母、数字、`-`，2–32 个字符。
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    (2..=32).contains(&b.len()) && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-') && !name.ends_with('-')
}

/// 请求路径属于哪个插件：`/api/plugins/<名>/…` 或 `/p/<名>/…`（也认不带结尾 `/` 的 `/p/<名>`）。
pub fn plugin_of_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(API_PREFIX).or_else(|| path.strip_prefix(PAGE_PREFIX))?;
    let name = rest.split(['/', '?']).next()?;
    valid_name(name).then_some(name)
}

/// 插件页面地址 `/p/<名>/<path>`（`path` 是清单里相对页面前缀的路径）。
pub fn page_url(name: &str, path: &str) -> String {
    format!("{PAGE_PREFIX}{name}/{}", path.trim_start_matches('/'))
}

/// 插件接口地址 `/api/plugins/<名>/<path>`。
pub fn api_url(name: &str, path: &str) -> String {
    format!("{API_PREFIX}{name}/{}", path.trim_start_matches('/'))
}

// ---------------------------------------------------------------- 清单

/// 中英文文案（英文缺省时回退中文）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Localized {
    #[serde(rename = "zh-CN")]
    pub zh: String,
    #[serde(rename = "en-US", default, skip_serializing_if = "String::is_empty")]
    pub en: String,
}

impl Localized {
    pub fn get(&self, locale: &str) -> &str {
        if locale.starts_with("en") && !self.en.is_empty() {
            &self.en
        } else {
            &self.zh
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

impl ProtocolRange {
    pub fn contains(&self, v: u32) -> bool {
        self.min <= v && v <= self.max
    }
}

/// “工具”菜单里的一项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: Localized,
    /// lucide 图标名（`globe`、`monitor` …），看看不认识就用默认图标
    #[serde(default)]
    pub icon: String,
    /// 相对页面前缀 `/p/<名>/` 的路径，空串就是插件首页
    #[serde(default)]
    pub path: String,
}

/// 对话里点链接时“用插件打开”（docs/PLUGIN_PROTOCOL.md §open_url）。路径相对接口前缀 `/api/plugins/<名>/`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenUrlCapability {
    /// GET → [`OpenUrlTargets`]
    pub targets: String,
    /// POST [`OpenUrlRequest`] → [`OpenUrlResponse`]
    pub open: String,
    /// 菜单里这一组的名字（如“远程浏览器”）
    pub title: Localized,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_url: Option<OpenUrlCapability>,
}

/// 只读访问（[`HDR_READONLY`]）时看看在网关层怎么处理，不依赖插件自觉检查。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadonlyPolicy {
    /// 一律拒绝（默认）
    #[default]
    Deny,
    /// 只放行 GET / HEAD 和 WebSocket 以外的读请求
    ReadMethods,
    /// 全部转给插件，由插件按 [`HDR_READONLY`] 自己限制
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub protocol: ProtocolRange,
    /// 需要的最低看看版本（semver）
    #[serde(default)]
    pub min_client: String,
    /// 支持的平台（`linux-x86_64` 等，与客户端更新接口的平台标识一致）
    pub platforms: Vec<String>,
    /// 可执行文件，相对包根目录
    pub exec: String,
    pub title: Localized,
    #[serde(default)]
    pub description: Localized,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// 相对插件数据目录的文件：看看启动时它存在就立即拉起插件（插件借此接回看看重启前还在跑的后台任务）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart_if_exists: Option<String>,
    #[serde(default)]
    pub readonly_policy: ReadonlyPolicy,
    /// 相对页面前缀的路径前缀：这些 HTML 页面由看看注入 `/static/web.js`（远程 WebRTC 时页面里的 WebSocket 经启动页走）
    #[serde(default)]
    pub inject_web_prefixes: Vec<String>,
    /// 没有请求、也不忙（[`PluginStatus::busy`]）多久后停掉插件进程；0 = 不停
    #[serde(default = "default_idle")]
    pub idle_stop_minutes: u32,
    /// 需要平台授权才能下载（目录里显示授权情况）
    #[serde(default)]
    pub requires_license: bool,
}

fn default_idle() -> u32 {
    10
}

fn safe_rel(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && !p.contains('\\') && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let m: Manifest = serde_json::from_slice(bytes).map_err(|e| format!("manifest: {e}"))?;
        m.validate().map_err(|f| format!("manifest: invalid {f}"))?;
        Ok(m)
    }

    /// 字段检查，返回不合格的字段名。
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_name(&self.name) {
            return Err("name");
        }
        if self.version.is_empty() || self.version.len() > 64 {
            return Err("version");
        }
        if self.protocol.min > self.protocol.max {
            return Err("protocol");
        }
        if !safe_rel(&self.exec) {
            return Err("exec");
        }
        if self.title.zh.is_empty() {
            return Err("title");
        }
        if self.platforms.is_empty() {
            return Err("platforms");
        }
        if self.entries.iter().any(|e| e.id.is_empty() || e.title.zh.is_empty() || e.path.contains("..")) {
            return Err("entries");
        }
        if self.autostart_if_exists.as_deref().is_some_and(|p| !safe_rel(p)) {
            return Err("autostart_if_exists");
        }
        if self.inject_web_prefixes.iter().any(|p| p.contains("..")) {
            return Err("inject_web_prefixes");
        }
        if let Some(c) = &self.capabilities.open_url {
            if c.targets.contains("..") || c.open.contains("..") {
                return Err("capabilities");
            }
        }
        Ok(())
    }

    /// 看看（协议 `v`）能不能运行它。
    pub fn supports_protocol(&self, v: u32) -> bool {
        self.protocol.contains(v)
    }

    pub fn supports_platform(&self, target: &str) -> bool {
        self.platforms.iter().any(|p| p == target)
    }

    /// 这个页面路径（完整路径，如 `/p/browser/view/`）要不要注入 web.js。
    pub fn wants_web_inject(&self, path: &str) -> bool {
        let base = page_url(&self.name, "");
        let Some(rel) = path.strip_prefix(&base) else { return false };
        self.inject_web_prefixes.iter().any(|p| rel.starts_with(p.trim_start_matches('/')))
    }
}

// ---------------------------------------------------------------- 状态

/// [`PLUGIN_STATUS_PATH`] 的响应。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginStatus {
    /// 插件管的东西在跑（如浏览器开着）：界面上的绿点
    #[serde(default)]
    pub running: bool,
    /// 忙：看看不能因为闲置停掉插件，也不在这时更新它
    #[serde(default)]
    pub busy: bool,
    /// 菜单上显示的一小段文字（如“运行中”），插件按 [`HDR_LOCALE`] 给
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    /// 插件自定义的其他内容，看看原样交给界面
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

// ---------------------------------------------------------------- open_url

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenUrlTarget {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// 正在用（排第一、菜单上标出来）
    #[serde(default)]
    pub active: bool,
    /// 不指定目标时用它
    #[serde(default)]
    pub default: bool,
}

/// GET targets 的响应。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenUrlTargets {
    /// 现在能打开（例如浏览器内核已装好）；false 时菜单只给“去设置”（跳到 `setup_path`）
    pub ready: bool,
    #[serde(default)]
    pub targets: Vec<OpenUrlTarget>,
    /// 没准备好时跳去的页面（相对页面前缀）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_path: Option<String>,
    /// 没准备好时菜单上的提示（如“去安装”）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_hint: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenUrlRequest {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// 用户已经同意的确认（[`OpenUrlResponse::Confirm`] 的 `id`）
    #[serde(default)]
    pub confirmed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum OpenUrlResponse {
    /// 已经打开；`navigate`：随后让管理台跳到这个插件页面（相对页面前缀），不给就不跳
    Opened {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        navigate: Option<String>,
    },
    /// 要用户先确认（如“会关掉正在用的那套配置”）：看看弹确认框，同意后把 `id` 放进 `confirmed` 重发
    Confirm {
        id: String,
        title: String,
        body: String,
        confirm: String,
        #[serde(default)]
        danger: bool,
    },
    /// 打开前先跳到插件页面（例如还在启动、要显示进度），插件自己接着打开
    Navigate { navigate: String },
}

// ---------------------------------------------------------------- 宿主接口

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub protocol: u32,
    pub client_version: String,
    /// 平台标识（`linux-x86_64` 等）
    pub target: String,
    pub logged_in: bool,
    /// 这台电脑的设备 public id（登录时）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// 平台地址（登录时）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// 平台当前时间（可信时间，毫秒）：插件校时用，配合自己的单调时钟防回拨
    pub server_time_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostNotify {
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// 点提醒打开的页面（相对页面前缀）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

// ---------------------------------------------------------------- 目录与下载

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub version: String,
    pub title: Localized,
    #[serde(default)]
    pub description: Localized,
    #[serde(default)]
    pub icon: String,
    pub platforms: Vec<String>,
    /// 这个平台的安装包大小与 SHA-256（不支持这个平台时为 0 / 空）
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha256: String,
    pub requires_license: bool,
    /// 本账户有没有授权（不需要授权的插件恒为 true；没登录时为 false）
    pub licensed: bool,
    pub protocol: ProtocolRange,
    #[serde(default)]
    pub min_client: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCatalog {
    pub plugins: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub target: String,
}

/// 下载票据：`url` 是短时有效的地址（平台签发，不需要再签名），下载完核对 `sha256`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadTicket {
    pub version: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

// ---------------------------------------------------------------- 包签名（离线私钥）

/// [`PACKAGE_FILE`]：包里每个文件的 SHA-256（不含 [`PACKAGE_FILE`] 和 [`SIGNATURE_FILE`] 自己）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageIndex {
    pub name: String,
    pub version: String,
    pub target: String,
    /// 相对路径 → SHA-256 hex
    pub files: BTreeMap<String, String>,
    /// 可执行的文件（解包后设 0755）
    #[serde(default)]
    pub executables: Vec<String>,
}

impl PackageIndex {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let p: PackageIndex = serde_json::from_slice(bytes).map_err(|e| format!("package: {e}"))?;
        if !valid_name(&p.name) || p.files.is_empty() {
            return Err("package: invalid".into());
        }
        if !p.files.contains_key(MANIFEST_FILE) {
            return Err("package: no manifest".into());
        }
        for (path, hash) in &p.files {
            if !safe_rel(path) || path == PACKAGE_FILE || path == SIGNATURE_FILE || hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("package: bad entry {path}"));
            }
        }
        if p.executables.iter().any(|e| !p.files.contains_key(e)) {
            return Err("package: bad executables".into());
        }
        Ok(p)
    }
}

/// [`SIGNATURE_FILE`] 的内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSignature {
    pub kid: String,
    pub sig: String,
}

/// 包签名规范串 `LL1-PLUGIN-PKG`：签的是 [`PACKAGE_FILE`] 的原始字节。
pub fn package_canonical(index_bytes: &[u8]) -> String {
    format!("LL1-PLUGIN-PKG\n{}", sha256_hex(index_bytes))
}

pub fn sign_package(key: &SigningKey, kid: &str, index_bytes: &[u8]) -> PackageSignature {
    PackageSignature { kid: kid.to_string(), sig: sign(key, &package_canonical(index_bytes)) }
}

pub fn verify_package(keys: &HashMap<String, VerifyingKey>, index_bytes: &[u8], sig: &PackageSignature) -> bool {
    keys.get(&sig.kid).is_some_and(|k| verify(k, &package_canonical(index_bytes), &sig.sig))
}

// ---------------------------------------------------------------- 授权凭证（平台签名）

/// 插件申请凭证时交给看看的内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseRequest {
    /// 机器指纹摘要（SHA-256 hex）：插件按 machine-id 等算，容器里没有固定 machine-id 时用自己数据卷里的安装 ID
    pub machine: String,
    /// 插件版本（平台记录）
    #[serde(default)]
    pub version: String,
}

pub const LICENSE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseClaims {
    pub v: u32,
    pub plugin: String,
    /// 账户 id（平台内部的公开 id）
    pub account: String,
    /// 设备 public id
    pub device: String,
    pub machine: String,
    pub issued_ms: i64,
    pub expires_ms: i64,
    /// 过了这个时间就该续期（一般是有效期过半）
    pub renew_after_ms: i64,
    /// 追查用的水印（每次签发不同，平台记着是谁）
    pub watermark: String,
    /// 解开 [`PluginData`] 的密钥（base64url，32 字节）：同一账户 × 设备 × 插件不变，续期后缓存的数据照样能解
    pub data_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedLicense {
    pub kid: String,
    /// base64url(JSON [`LicenseClaims`])
    pub payload: String,
    pub sig: String,
}

pub fn license_canonical(payload_b64: &str) -> String {
    format!("LL1-PLUGIN-LIC\n{payload_b64}")
}

pub fn sign_license(key: &SigningKey, kid: &str, claims: &LicenseClaims) -> SignedLicense {
    let payload = b64(&serde_json::to_vec(claims).expect("license json"));
    let sig = sign(key, &license_canonical(&payload));
    SignedLicense { kid: kid.to_string(), payload, sig }
}

/// 验签并取出内容（不检查时间：调用方按自己的可信时间判断 `expires_ms`）。
pub fn verify_license(keys: &HashMap<String, VerifyingKey>, lic: &SignedLicense, plugin: &str) -> Option<LicenseClaims> {
    let key = keys.get(&lic.kid)?;
    if !verify(key, &license_canonical(&lic.payload), &lic.sig) {
        return None;
    }
    let claims: LicenseClaims = serde_json::from_slice(&b64_decode(&lic.payload)?).ok()?;
    (claims.v == LICENSE_VERSION && claims.plugin == plugin).then_some(claims)
}

// ---------------------------------------------------------------- 按凭证下发的数据

/// [`HOST_DATA_PATH`] / [`client_data_path`] 的响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginData {
    /// 数据版本（插件据此判断缓存是否过时）
    pub version: String,
    /// 水印（与凭证里的对应）
    pub watermark: String,
    /// [`seal`] 的结果
    pub blob: String,
}

/// 用 `data_key` 加密：base64url(nonce(24) || ciphertext)。
pub fn seal(key_b64: &str, plain: &[u8], nonce: [u8; 24]) -> Option<String> {
    let key: [u8; 32] = b64_decode(key_b64)?.try_into().ok()?;
    let ct = XChaCha20Poly1305::new(&key.into()).encrypt(XNonce::from_slice(&nonce), plain).ok()?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Some(b64(&out))
}

pub fn open(key_b64: &str, blob: &str) -> Option<Vec<u8>> {
    let key: [u8; 32] = b64_decode(key_b64)?.try_into().ok()?;
    let raw = b64_decode(blob)?;
    if raw.len() < 24 + 16 {
        return None;
    }
    let (nonce, ct) = raw.split_at(24);
    XChaCha20Poly1305::new(&key.into()).decrypt(XNonce::from_slice(nonce), ct).ok()
}

// ---------------------------------------------------------------- 界面消息（postMessage）

/// 管理台与插件 iframe 之间的 `postMessage`：`{ "ll": "plugin", "v": 1, "type": …, … }`。
/// 管理台 → 插件：`init`（theme、locale、device、conn）、`theme`、`locale`、`device`、`conn`、`back`（手机返回键）；
/// 插件 → 管理台：`ready`、`title`（text）、`fullscreen`（on）、`back-result`（handled）、`navigate`（to：管理台路径）、`toast`（text、kind）。
pub const UI_MESSAGE_TAG: &str = "plugin";

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            name: "browser".into(),
            version: "1.0.0".into(),
            protocol: ProtocolRange { min: 1, max: 1 },
            min_client: "1.87.0".into(),
            platforms: vec!["linux-x86_64".into()],
            exec: "bin/looklook-browser".into(),
            title: Localized { zh: "远程浏览器".into(), en: "Remote browser".into() },
            description: Localized::default(),
            icon: "globe".into(),
            entries: vec![Entry { id: "main".into(), title: Localized { zh: "远程浏览器".into(), en: String::new() }, icon: "globe".into(), path: String::new() }],
            capabilities: Capabilities::default(),
            autostart_if_exists: Some("run.json".into()),
            readonly_policy: ReadonlyPolicy::Deny,
            inject_web_prefixes: vec!["view/".into()],
            idle_stop_minutes: 10,
            requires_license: true,
        }
    }

    #[test]
    fn names_and_paths() {
        assert!(valid_name("browser") && valid_name("a1-b"));
        assert!(!valid_name("B") && !valid_name("1a") && !valid_name("a") && !valid_name("a-") && !valid_name("a_b"));
        assert_eq!(plugin_of_path("/api/plugins/browser/start"), Some("browser"));
        assert_eq!(plugin_of_path("/p/browser/view/?x=1"), Some("browser"));
        assert_eq!(plugin_of_path("/p/browser"), Some("browser"));
        assert_eq!(plugin_of_path("/p/../x"), None);
        assert_eq!(plugin_of_path("/api/instances/x"), None);
        assert_eq!(page_url("browser", "/view/"), "/p/browser/view/");
        assert_eq!(api_url("browser", "open-url"), "/api/plugins/browser/open-url");
    }

    #[test]
    fn manifest_roundtrip_and_checks() {
        let m = manifest();
        let bytes = serde_json::to_vec(&m).unwrap();
        assert_eq!(Manifest::parse(&bytes).unwrap(), m);
        assert!(m.supports_protocol(1) && !m.supports_protocol(2));
        assert!(m.wants_web_inject("/p/browser/view/") && !m.wants_web_inject("/p/browser/") && !m.wants_web_inject("/p/other/view/"));
        let mut bad = m.clone();
        bad.exec = "../x".into();
        assert_eq!(bad.validate(), Err("exec"));
        // 缺省字段
        let v = serde_json::json!({"name":"x1","version":"1","protocol":{"min":1,"max":1},"platforms":["linux-x86_64"],"exec":"x","title":{"zh-CN":"X"}});
        let m2 = Manifest::parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(m2.idle_stop_minutes, 10);
        assert_eq!(m2.readonly_policy, ReadonlyPolicy::Deny);
    }

    #[test]
    fn package_signature() {
        let k = SigningKey::from_bytes(&[3u8; 32]);
        let mut files = BTreeMap::new();
        files.insert(MANIFEST_FILE.to_string(), sha256_hex(b"{}"));
        let idx = PackageIndex { name: "browser".into(), version: "1".into(), target: "linux-x86_64".into(), files, executables: vec![] };
        let bytes = serde_json::to_vec(&idx).unwrap();
        assert!(PackageIndex::parse(&bytes).is_ok());
        let sig = sign_package(&k, "k1", &bytes);
        let keys = HashMap::from([("k1".to_string(), k.verifying_key())]);
        assert!(verify_package(&keys, &bytes, &sig));
        assert!(!verify_package(&keys, b"{}", &sig));
        assert!(!verify_package(&HashMap::new(), &bytes, &sig));
        let mut evil = idx.clone();
        evil.files.insert("../x".into(), sha256_hex(b""));
        assert!(PackageIndex::parse(&serde_json::to_vec(&evil).unwrap()).is_err());
    }

    #[test]
    fn license_and_data() {
        let k = SigningKey::from_bytes(&[5u8; 32]);
        let key = b64(&[9u8; 32]);
        let c = LicenseClaims {
            v: LICENSE_VERSION,
            plugin: "browser".into(),
            account: "u1".into(),
            device: "d1".into(),
            machine: "m".into(),
            issued_ms: 1,
            expires_ms: 2,
            renew_after_ms: 1,
            watermark: "w".into(),
            data_key: key.clone(),
        };
        let lic = sign_license(&k, "p1", &c);
        let keys = HashMap::from([("p1".to_string(), k.verifying_key())]);
        assert_eq!(verify_license(&keys, &lic, "browser"), Some(c));
        assert_eq!(verify_license(&keys, &lic, "other"), None);
        let mut t = lic.clone();
        t.payload.push('A');
        assert_eq!(verify_license(&keys, &t, "browser"), None);
        let blob = seal(&key, b"hello", [1u8; 24]).unwrap();
        assert_eq!(open(&key, &blob).as_deref(), Some(&b"hello"[..]));
        assert_eq!(open(&b64(&[8u8; 32]), &blob), None);
    }

    #[test]
    fn open_url_json() {
        let r = OpenUrlResponse::Confirm { id: "switch".into(), title: "t".into(), body: "b".into(), confirm: "c".into(), danger: false };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["result"], "confirm");
        let o: OpenUrlResponse = serde_json::from_str(r#"{"result":"opened"}"#).unwrap();
        assert_eq!(o, OpenUrlResponse::Opened { navigate: None });
    }
}
