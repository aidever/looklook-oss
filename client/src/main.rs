//! 看看客户端（Looklook client）。
//!
//! ```text
//! looklook                 启动（默认），并在桌面系统上打开管理台
//! looklook open            在浏览器中打开管理台
//! looklook url             打印管理台地址（本机与局域网）
//! looklook access          查看或修改访问控制（局域网访问、白名单、访问码）
//! looklook login           授权登录（命令行显示授权码、网址与二维码，可在手机上批准）
//! looklook logout          退出登录
//! looklook status          查看状态
//! looklook update          检查并安装新版本（别名 upgrade）
//! ```
//!
//! 命令行的帮助与输出是中英双语的：没有图形界面的服务器上没有界面语言可选。
//!
//! Windows / macOS 上直接启动（双击图标、开始菜单、程序坞）是桌面应用：应用窗口显示管理台，
//! 系统托盘常驻（`src/desktop.rs`）。Windows 发布构建（release，非 debug）去掉控制台窗口
//! （任务 7a），从终端运行时会自动重新接上父进程的控制台（见 `hardening.rs`），命令行子命令一样能用。

// 只在 Windows 的 release 构建去掉控制台；dev 构建（`cargo run`）保留控制台方便调试，
// Linux/macOS 本来就不受这个属性影响（`windows_subsystem` 是 Windows 特有概念，见
// docs/FAQ.md 里 macOS 那节的说明：Finder/LaunchAgent 启动本来就没有控制台）。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod attach;
mod shared_pcs;
mod native_term;
mod account;
mod agent_update;
mod agents;
mod assistant;
mod assistant_index;
mod assistant_jobs;
mod assistant_memory;
mod archive;
mod chat;
mod checkpoint;
pub mod agy_profiles;
mod claude_profiles;
mod claude_resets;
mod codex_profiles;
mod clock;
mod diag;
mod direct_tls;
mod docker;
mod error;
mod feishu;
mod fsops;
mod gateway;
mod git;
mod guard;
mod history;
mod hardening;
mod instances;
mod lark_cli;
mod listen;
mod metrics;
mod model;
mod model_presets;
mod model_tuning;
mod model_test;
mod notify;
mod office;
mod fsview;
mod textfile;
mod feishu_bot;
mod paths;
mod plugins;
mod desk_pet;
mod pet_github;
mod pet_lines;
mod pet_linesets;
mod pet_transfer;
mod pets;
mod platform;
mod ports;
mod proctree;
mod session_probe;
mod idle_stop;
mod claude_prices;
mod pricing;
mod prompts;
mod provider_quota;
mod push;
mod relay;
mod relay_watch;
mod remote_browser;
mod rtc;
mod run_actions;
mod settings;
mod settings_sync;
mod shells;
mod skills;
mod ssh;
mod ssh_transfer;
mod store;
mod tips;
mod termd_client;
mod tools;
mod transfer;
#[cfg(any(windows, target_os = "macos"))]
mod desktop;
mod updater;
mod usage_stats;
mod util;
mod versions;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::account::Account;
use crate::gateway::Inner;
use crate::instances::Instances;
use crate::paths::Paths;
use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "looklook",
    version,
    about = "看看客户端：随时随地操作你的电脑和 AI 助手\nLooklook client: reach your computer and AI assistants from anywhere",
    after_help = HELP_EXAMPLES
)]
struct Cli {
    /// 数据目录（默认按系统惯例）/ Data directory (system default if omitted)
    #[arg(long, env = "LOOKLOOK_HOME", global = true)]
    home: Option<PathBuf>,
    /// 管理台监听地址，默认 0.0.0.0:1234（被占用时依次 +10）/ Console address, default 0.0.0.0:1234 (steps up by 10 if taken)
    #[arg(long, env = "LOOKLOOK_LISTEN", global = true)]
    listen: Option<SocketAddr>,
    /// 也当作这台电脑的来源 IP 或网段，逗号分隔（默认只有 127.0.0.1）；Docker 里运行时填宿主机连进来的网关，如 172.17.0.1
    /// Also treat these source IPs or ranges as this computer, comma-separated (default: only 127.0.0.1); in Docker, the gateway the host connects through, e.g. 172.17.0.1
    #[arg(long, env = "LOOKLOOK_LOCAL_FROM", global = true, value_delimiter = ',', value_parser = parse_local_from)]
    local_from: Vec<(IpAddr, u8)>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

const HELP_EXAMPLES: &str = "\
没有图形界面的服务器 / Servers without a desktop:
  looklook login            登录：显示授权码和二维码，用手机或任意浏览器批准
                            Sign in: shows a code and QR code to approve from your phone or any browser
  looklook login --key LLK-…
                            用安装密钥直接登录（云服务器、开机脚本；在网页“我的电脑”页生成）
                            Sign in with an install key (cloud servers, startup scripts)
  looklook status           查看登录状态 / Show sign-in status
  looklook upgrade          升级到最新版本 / Upgrade to the latest version
  looklook access lan on    允许局域网设备打开管理台 / Let LAN devices open the console
  looklook rtc              远程连接直连设置：端口范围、公网地址 / Direct connection settings: ports, public addresses
  looklook --local-from 172.17.0.1
                            在 Docker 里运行：宿主机上打开管理台也算这台电脑
                            In Docker: opening the console from the host counts as this computer

子命令的帮助 / Help for a command: looklook <command> -h";

#[derive(Subcommand)]
enum Cmd {
    /// 启动客户端（默认）/ Start the client (default)
    Run {
        /// 看看服务端地址（仅未登录时生效）/ Looklook server URL (only while signed out)
        #[arg(long, env = "LOOKLOOK_SERVER")]
        server: Option<String>,
        /// 启动后不打开浏览器 / Don't open the browser
        #[arg(long)]
        no_browser: bool,
    },
    /// 在浏览器中打开管理台 / Open the console in the browser
    Open,
    /// 打印管理台地址 / Print the console address
    Url,
    /// 查看或修改访问控制 / Show or change who may open the console
    Access {
        #[command(subcommand)]
        action: Option<AccessCmd>,
    },
    /// 远程连接直连设置：传输端口范围、公网 IPv4 / IPv6 / Direct connection settings: port range, public IPv4 / IPv6
    Rtc {
        #[command(subcommand)]
        action: Option<RtcCmd>,
    },
    /// 公网 HTTPS 直连：自己申请公网 IP 的证书，浏览器直接打开 https://IP:端口 / Public HTTPS: get a certificate for the public IP and open https://IP:port directly
    Https {
        #[command(subcommand)]
        action: Option<HttpsCmd>,
    },
    /// 登录看看账户（在手机或任意浏览器上批准）/ Sign in (approve from your phone or any browser)
    Login {
        /// 安装密钥（网页“电脑”页生成，LLK- 开头），不用到浏览器里批准 / Install key (LLK-…, made on the Devices page); no browser approval needed
        #[arg(long, env = "LOOKLOOK_INSTALL_KEY", hide_env_values = true)]
        key: Option<String>,
    },
    /// 退出登录 / Sign out
    Logout {
        /// 连不上服务器时也清除本机登录 / Clear the local sign-in even if the server can't be reached
        #[arg(long)]
        force: bool,
    },
    /// 查看状态 / Show status
    Status,
    /// 显示本机机器指纹（申请插件离线授权码用）/ Show this computer's fingerprint (for plugin offline license codes)
    Fingerprint,
    /// 填插件离线授权码；不填则列出已填的 / Enter a plugin offline license code; lists entered codes when omitted
    License {
        /// 管理员按机器指纹生成的授权码 / The code an admin generated for this fingerprint
        code: Option<String>,
    },
    /// 重启客户端，终端里的任务不受影响 / Restart the client; running tasks keep running
    Restart,
    /// 检查并安装新版本，终端里的任务不受影响 / Check for and install a new version; running tasks keep running
    #[command(visible_alias = "upgrade")]
    Update {
        /// 只检查，不安装 / Only check, don't install
        #[arg(long)]
        check: bool,
    },
    /// 连接在看看里保存的 SSH 主机（密码由看看提供，不用输入、不会显示）/ Connect to a saved SSH host
    Ssh {
        /// 主机名称或 id；不填则列出全部 / Host name or id; lists hosts when omitted
        host: Option<String>,
        /// 在主机上运行的命令（放在 -- 之后）/ Command to run on the host (after --)
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// 在当前终端窗口里接上一个终端会话（行首按 ~. 断开，会话继续运行）；不填则列出全部 / Attach to a terminal session here (type ~. at line start to detach)
    Attach {
        /// 终端名称或 id / Terminal name or id
        target: Option<String>,
        /// 会话在同一账户的另一台电脑上：那台电脑的设备 id（经本机看看转过去，target 要写会话 id）/ The session is on another computer (device id)
        #[arg(long)]
        device: Option<String>,
        /// 本地预测回显：auto（往返慢时开）、on、off / Local echo prediction
        #[arg(long, default_value = "auto")]
        predict: attach::PredictMode,
    },
    /// ssh 的 ProxyCommand：经跳板机连接时由看看自己连上跳板机（内部使用）/ ProxyCommand for jump hosts (internal)
    #[command(hide = true)]
    SshProxy { jump: String, host: String, port: u16 },
    /// 对话宠物包：检查、安装、列出（docs/PET_PACK_GUIDE.md）/ Chat pet packs: check, install, list
    Pet {
        #[command(subcommand)]
        action: PetCmd,
    },
    /// 安装脚本用（Linux）：把安装包目录放进数据目录的 versions/，删掉更高的版本，稳定入口 bin/looklook 指向它
    /// For installers (Linux): put the package into versions/ in the data directory and point bin/looklook at it
    #[command(hide = true)]
    InstallVersion { from: PathBuf },
}

#[derive(Subcommand)]
enum PetCmd {
    /// 检查一个宠物包文件夹（装了 ffmpeg 时还会看尺寸、透明通道、首尾帧）/ Check a pet pack folder
    Check { dir: PathBuf },
    /// 安装 .llpet / .zip、解开的宠物文件夹，或 GitHub 仓库（owner/repo、仓库链接；同 id 覆盖）/ Install a .llpet / .zip, a pet folder or a GitHub repo (replaces the same id)
    Install {
        /// 文件 / 文件夹，或 GitHub 仓库：owner/repo、owner/repo@分支、https://github.com/owner/repo/tree/main/pets/cat
        source: String,
        /// 仓库里有好几只时装哪几只（id 或仓库里的文件夹，逗号分隔）/ Which pets to install from a multi-pet repo (ids or folders, comma-separated)
        #[arg(long, value_delimiter = ',')]
        pick: Vec<String>,
        /// 仓库里的全装 / Install every pet in the repo
        #[arg(long)]
        all: bool,
    },
    /// 列出这台电脑上找得到的宠物 / List the pets found on this computer
    List,
}

#[derive(Subcommand)]
enum AccessCmd {
    /// 允许 / 不允许局域网设备打开管理台 / Allow or block LAN devices
    Lan {
        state: OnOff,
        /// 安装脚本用：只在第一次安装（还没有数据）时设置 / For installers: only applies on a fresh install
        #[arg(long, hide = true)]
        initial: bool,
    },
    /// 访问码：on 开启、off 关闭、new 换新的 / Access code: on, off, or new
    Code { action: CodeAction },
    /// 把 IP、网段或域名加入白名单 / Add an IP, range or domain to the whitelist (e.g. 203.0.113.0/24, home.example.com)
    Allow { ip: String },
    /// 从白名单移除 / Remove from the whitelist
    Remove { ip: String },
    /// 自定义监听端口（重启后生效）：如 8080，或 auto 恢复自动 / Custom listening port (takes effect after restart): e.g. 8080, or `auto`
    Port { value: String },
}

/// 改了只对新建的连接生效。客户端没在运行时直接改本机设置，下次启动生效。
/// Changes apply to new connections. If the client isn't running, the setting is saved for its next start.
#[derive(Subcommand)]
enum RtcCmd {
    /// 传输端口范围（UDP），如 50000-50031；default 恢复默认 / UDP port range, e.g. 50000-50031; `default` to reset
    Ports { range: String },
    /// 公网 IPv4 或域名（完全锥形 NAT、同号端口转发时填；动态 IP 填 DDNS 域名）；off 清除
    /// Public IPv4 or domain (full-cone NAT or same-port forwarding; DDNS domain for a dynamic IP); `off` to clear
    Ipv4 { addr: String },
    /// 公网 IPv6 或域名；off 清除 / Public IPv6 or domain; `off` to clear
    Ipv6 { addr: String },
    /// 使用 IPv6 / Use IPv6
    Ipv6Enabled { state: OnOff },
    /// 直连自检：从外面能不能打进端口段、NAT 类型（会访问主服务、中转和 cip.cc、ip.me 等服务）；退出码 0 = 可以直连 / Direct-connect self-check: whether the port range is reachable from outside and the NAT type (asks the server, relays and sites like cip.cc, ip.me); exit code 0 = direct works
    Check,
    /// 检测这台电脑的公网地址（会访问中转和 cip.cc、ip.me 等服务）/ Detect public addresses (asks the relay and sites like cip.cc, ip.me)
    Detect {
        /// 把检测到的地址保存下来 / Save the detected addresses
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum HttpsCmd {
    /// 开启（没填公网 IPv4 时先自动检测并填上）/ Turn on (detects and fills in the public IPv4 if it isn't set)
    On {
        /// 等证书申请完再返回（最多 2 分钟）/ Wait until the certificate is issued (up to 2 minutes)
        #[arg(long)]
        wait: bool,
    },
    /// 关闭 / Turn off
    Off,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum CodeAction {
    On,
    Off,
    New,
}

fn main() -> Result<()> {
    // Linux：数据目录里有更高的版本时换成它运行（src/versions.rs）。最先做：hook、askpass 在内的所有子命令都和客户端用同一个版本
    versions::jump();
    let _ = versions::entry(); // 跳转过来的：记下原来的入口，不传给终端里的程序
    // Claude Code 调用的 hook（对话界面，src/chat/hooks.rs）：最先处理，不做别的初始化，启动要快
    {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some(chat::hooks::SUBCOMMAND) {
            std::process::exit(chat::hooks::main(&args.next().unwrap_or_default()));
        }
    }
    // ssh 把看看当作 SSH_ASKPASS 调用（`<looklook> "<提示>"`）：去找发起连接的 `looklook ssh` 要密码（src/ssh.rs）。
    // 必须在接回控制台之前：Windows 上 ssh 用管道读 askpass 的标准输出，换成控制台的话密码就写到屏幕上、ssh 读到空的
    if let Ok(env) = std::env::var(ssh::ASKPASS_ENV) {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.len() <= 1 {
            std::process::exit(ssh::askpass_main(&env, args.first().map(String::as_str).unwrap_or("")));
        }
    }
    // Windows release 构建去掉了控制台（见上面的 `windows_subsystem` 属性）：从真正的终端
    // 启动时把标准输入输出接回父进程的控制台，命令行子命令才看得见输出；双击/开机启动等
    // 没有父控制台的场景这个调用是无害的空操作。其它平台是空实现。
    hardening::attach_console_if_launched_from_terminal();
    let _ = updater::exe_path(); // 记下程序路径（更新替换文件后重启要用）
    let cli = Cli::parse();
    gateway::access::set_local_from(cli.local_from.clone());
    let rt = tokio::runtime::Runtime::new()?;
    // 命令行子命令优先读正在运行的客户端写下的 run/endpoint，这里只是找不到时的后备
    let cli_listen = cli.listen.unwrap_or(listen::DEFAULT);
    match cli.cmd {
        None => dispatch_run(rt, cli.home, cli.listen, None, false),
        Some(Cmd::Run { server, no_browser }) => dispatch_run(rt, cli.home, cli.listen, server, no_browser),
        Some(Cmd::Open) => open_ui(&local_url(&cli.home, cli_listen)?),
        Some(Cmd::Url) => {
            println!("{}", local_url(&cli.home, cli_listen)?);
            if let Ok(v) = rt.block_on(call(&cli.home, cli_listen, "GET", "/api/access", None)) {
                if let (true, Some(ip)) = (v["allow_lan"].as_bool() == Some(true), v["lan_ips"][0].as_str()) {
                    println!("局域网 / LAN: http://{}:{}/", ip, v["port"]);
                }
            }
            Ok(())
        }
        Some(Cmd::Access { action: Some(AccessCmd::Lan { state, initial: true }) }) => {
            // 安装脚本在启动服务之前调用：客户端还没运行，直接写数据库
            let paths = Paths::discover(cli.home)?;
            if paths.db().exists() {
                println!("unchanged");
                return Ok(());
            }
            let store = Store::open(&paths.db())?;
            let mut cfg = gateway::access::AccessConfig::load(&store);
            cfg.allow_lan = matches!(state, OnOff::On);
            cfg.save(&store)?;
            println!("{}", if cfg.allow_lan { "enabled" } else { "disabled" });
            Ok(())
        }
        Some(Cmd::Access { action }) => rt.block_on(cli_access(&cli.home, cli_listen, action)),
        Some(Cmd::Rtc { action }) => rt.block_on(cli_rtc(&cli.home, cli_listen, action)),
        Some(Cmd::Https { action }) => rt.block_on(cli_https(&cli.home, cli_listen, action)),
        Some(Cmd::Login { key }) => rt.block_on(cli_login(&cli.home, cli_listen, key)),
        Some(Cmd::Logout { force }) => {
            let v = rt.block_on(call(&cli.home, cli_listen, "POST", "/api/auth/logout", Some(json!({ "force": force }))))?;
            print_status(&v);
            Ok(())
        }
        Some(Cmd::Status) => {
            let v = rt.block_on(call(&cli.home, cli_listen, "GET", "/api/status", None))?;
            println!("看看客户端 / Looklook client: {}（运行中 / running）", v["version"].as_str().unwrap_or("-"));
            print_status(&v["account"]);
            if v["account"]["logged_in"].as_bool() == Some(true) {
                let r = &v["relay"];
                let state = match r["state"].as_str().unwrap_or("") {
                    "connected" => "已连接 / connected".to_string(),
                    "connecting" => "连接中 / connecting".to_string(),
                    "failed" => format!("连不上 / failed（{}）", r["reason"].as_str().or(r["detail"].as_str()).unwrap_or("-")),
                    other => other.to_string(),
                };
                println!("远程访问 / Remote access: {state}");
            }
            println!("终端方式 / Terminal backend: {}", v["capabilities"]["backend"].as_str().unwrap_or("-"));
            println!("本机地址 / Local URL: {}", local_url(&cli.home, cli_listen).unwrap_or_else(|_| "-".into()));
            if let Ok(a) = rt.block_on(call(&cli.home, cli_listen, "GET", "/api/access", None)) {
                print_access(&a);
            }
            Ok(())
        }
        Some(Cmd::Fingerprint) => {
            let paths = Paths::discover(cli.home)?;
            println!("{}", looklook_protocol::plugin::machine_fingerprint(&paths.home)?);
            Ok(())
        }
        Some(Cmd::License { code }) => {
            let v = match code {
                Some(c) => rt.block_on(call(&cli.home, cli_listen, "POST", "/api/plugin-store/offline", Some(json!({ "code": c }))))?,
                None => rt.block_on(call(&cli.home, cli_listen, "GET", "/api/plugin-store?local=1", None))?,
            };
            println!("机器指纹 / Fingerprint: {}", v["machine"].as_str().unwrap_or("-"));
            let items = v["offline"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("还没有离线授权码 / No offline license codes");
            }
            for i in items {
                let state = match i["state"].as_str().unwrap_or("") {
                    "valid" => "有效 / valid",
                    "expired" => "已过期 / expired",
                    "other_machine" => "不是这台电脑的 / for another computer",
                    _ => "无效 / invalid",
                };
                println!("{}  {}  {}", i["plugin"].as_str().unwrap_or("-"), i["expires_at"].as_str().unwrap_or("-"), state);
            }
            Ok(())
        }
        Some(Cmd::Restart) => rt.block_on(cli_restart(&cli.home, cli_listen)),
        Some(Cmd::Update { check }) => rt.block_on(cli_update(&cli.home, cli_listen, check)),
        Some(Cmd::Ssh { host, command }) => {
            let paths = Paths::discover(cli.home)?;
            let store = Store::open(&paths.db())?;
            match host {
                Some(h) => std::process::exit(ssh::connect_cli(&store, &paths.home, &h, &command)?),
                None => {
                    for h in ssh::list(&store) {
                        println!("{:<20} {}@{}:{}  {}{}", h.name, h.user, h.host, h.port, h.auth, if h.allow_ai { "  ai" } else { "" });
                    }
                    Ok(())
                }
            }
        }
        Some(Cmd::Pet { action }) => {
            let paths = Paths::discover(cli.home)?;
            match action {
                PetCmd::Check { dir } => std::process::exit(pets::check(&dir)),
                PetCmd::Install { source, pick, all } => {
                    let r = if pet_github::looks_like(&source) {
                        pet_github_cli(&rt, &paths.home, &source, &pick, all)
                    } else {
                        pets::import_path(&paths.home, Path::new(&source)).map(|p| vec![p])
                    };
                    match r {
                        Ok(ps) => {
                            for p in ps {
                                println!("✓ {} → {}", p.id, p.dir.display());
                            }
                            Ok(())
                        }
                        Err(e) => {
                            eprintln!("✗ {}: {}", e.code, e.detail);
                            std::process::exit(1)
                        }
                    }
                }
                PetCmd::List => {
                    for p in pets::scan(&paths.home) {
                        let err = p.error.as_ref().map(|e| format!("  ✗ {}: {}", e.code, e.detail)).unwrap_or_default();
                        println!("{:<28} {:<7} {}{}", p.key(), p.manifest.renderer, p.dir.display(), err);
                    }
                    Ok(())
                }
            }
        }
        Some(Cmd::InstallVersion { from }) => {
            let paths = Paths::discover(cli.home)?;
            let dir = versions::install_explicit(&paths.home, &from).map_err(anyhow::Error::msg)?;
            println!("{}", dir.display());
            Ok(())
        }
        Some(Cmd::Attach { target, device, predict }) => rt.block_on(async {
            if let Some(device) = device {
                let target = target.context("--device 要带会话 id / --device needs a session id")?;
                let addr = endpoint(&cli.home, cli_listen)?;
                std::process::exit(attach::run_remote(addr, &device, &target, predict).await?)
            }
            let list = call(&cli.home, cli_listen, "GET", "/api/instances", None).await?;
            let list = if list.is_array() { list } else { list["items"].clone() };
            let Some(target) = target else {
                attach::print_list(&list);
                return Ok(());
            };
            let addr = endpoint(&cli.home, cli_listen)?;
            let mux = Paths::discover(cli.home.clone()).ok().and_then(|p| p.mux);
            let code = attach::run(addr, &list, &target, mux, predict).await?;
            std::process::exit(code)
        }),
        Some(Cmd::SshProxy { jump, host, port }) => {
            let paths = Paths::discover(cli.home)?;
            let store = Store::open(&paths.db())?;
            std::process::exit(ssh::proxy_cli(&store, &paths.home, &jump, &host, port)?)
        }
    }
}

/// `looklook update`：让正在运行的客户端检查、下载并安装新版本，等它用新版本重新启动。
async fn cli_update(home: &Option<PathBuf>, listen: SocketAddr, check_only: bool) -> Result<()> {
    let u = call(home, listen, "GET", "/api/update", None).await?;
    let current = u["current"].as_str().unwrap_or("-").to_string();
    if u["available"].as_bool() != Some(true) {
        println!("已是最新版本 / Already up to date: {current}");
        return Ok(());
    }
    let version = u["version"].as_str().unwrap_or("-");
    println!("有新版本 / New version: {current} → {version}{}", if u["required"].as_bool() == Some(true) { "（必须更新 / required）" } else { "" });
    if let Some(notes) = u["notes"].as_str().filter(|n| !n.is_empty()) {
        println!("\n{notes}\n");
    }
    if check_only {
        println!("运行 looklook upgrade 安装。/ Run looklook upgrade to install it.");
        return Ok(());
    }
    if u["can_install"].as_bool() != Some(true) {
        let reason = u["cannot_reason"].as_str().unwrap_or("unsupported");
        bail!("不能在这里自动更新（{reason}），请到看看网页的“下载”页下载新版本手动安装\nCan't update automatically here ({reason}); download the new version from the Download page on the Looklook website");
    }
    call(home, listen, "POST", "/api/update/install", Some(json!({}))).await?;
    let mut last = String::new();
    // 下载 + 安装 + 重启，最多等 15 分钟
    for _ in 0..900 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        // 重启期间连不上是正常的
        let Ok(s) = call(home, listen, "GET", "/api/status", None).await else { continue };
        if s["version"].as_str().is_some_and(|v| v != current) {
            println!("已更新到 {0}，客户端已重新启动。/ Updated to {0}; the client has restarted.", s["version"].as_str().unwrap_or("-"));
            return Ok(());
        }
        let job = &s["update"]["job"];
        let line = match job["state"].as_str().unwrap_or("") {
            "downloading" => {
                let mb = |v: &Value| v.as_u64().map(|n| format!("{:.1} MB", n as f64 / 1048576.0));
                format!("正在下载 / Downloading… {} / {}", mb(&job["done"]).unwrap_or_default(), mb(&job["total"]).unwrap_or_else(|| "?".into()))
            }
            "installing" => "正在安装 / Installing…".into(),
            "restarting" => "正在重启 / Restarting…".into(),
            "failed" => {
                let detail = job["detail"].as_str().unwrap_or_default();
                let why = cli_error(job["code"].as_str().unwrap_or("UPDATE_FAILED"), &Value::Null);
                bail!("更新失败 / Update failed: {why}{}", if detail.is_empty() { String::new() } else { format!("（{detail}）") });
            }
            _ => continue,
        };
        if line != last {
            println!("{line}");
            last = line;
        }
    }
    bail!("等了 15 分钟还没完成，运行 looklook status 查看客户端状态\nNot finished after 15 minutes; run looklook status to check the client")
}

fn parse_local_from(s: &str) -> std::result::Result<(IpAddr, u8), String> {
    gateway::access::parse_net(s).map_err(|e| match e {
        "ip_too_wide" => "网段太大 / range too wide".to_string(),
        "ip_loopback" => "127.0.0.1 本来就算这台电脑 / loopback is always this computer".to_string(),
        _ => "不是 IP 或网段 / not an IP or range".to_string(),
    })
}

/// `None`（直接双击/开机启动）与 `looklook run` 走同一条路：Windows/macOS 是桌面应用（窗口 + 托盘，
/// 见 `src/desktop.rs`），Linux 没有托盘，跟以前一样直接跑在当前线程的 tokio 运行时上。
/// `looklook pet install <GitHub 仓库>`：只有一只直接装；好几只时要 `--pick` 或 `--all`，否则列出来
fn pet_github_cli(rt: &tokio::runtime::Runtime, home: &Path, input: &str, pick: &[String], all: bool) -> Result<Vec<pets::Pack>, pets::PackError> {
    let repo = pet_github::parse(input).ok_or_else(|| pets::perr("bad_repo", input))?;
    let tmp = tempfile::tempdir().map_err(|e| pets::perr("io", e.to_string()))?;
    let zip = tmp.path().join("repo.zip");
    eprintln!("↓ github.com/{}{}", repo.slug(), repo.reference.as_deref().map(|r| format!("@{r}")).unwrap_or_default());
    rt.block_on(pet_github::download(&repo, &zip))?;
    // 解到 pets/ 下面，装的时候同盘 rename
    let base = pets::local_dir(home);
    std::fs::create_dir_all(&base).map_err(|e| pets::perr("io", e.to_string()))?;
    let stage = tempfile::Builder::new().prefix(".gh-cli-").tempdir_in(&base).map_err(|e| pets::perr("io", e.to_string()))?;
    let found = pet_github::stage(&zip, &repo.path, stage.path())?;
    if found.len() > 1 && pick.is_empty() && !all {
        eprintln!("这个仓库里有 {} 只宠物，用 --pick <id,…> 挑几只，或 --all 全装 / {} pets in this repo; use --pick <id,…> or --all:", found.len(), found.len());
        for f in &found {
            match &f.pack {
                Ok(p) => eprintln!("  {:<24} {}", p.id, if f.path.is_empty() { "/" } else { &f.path }),
                Err(e) => eprintln!("  {:<24} {}  ✗ {}: {}", "?", f.path, e.code, e.detail),
            }
        }
        std::process::exit(2);
    }
    pet_github::install(home, &repo, found, pick)
}

fn dispatch_run(rt: tokio::runtime::Runtime, home: Option<PathBuf>, listen: Option<SocketAddr>, server: Option<String>, no_browser: bool) -> Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        desktop::run(rt, home, listen, server, no_browser)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        rt.block_on(run(home, listen, server, no_browser, CancellationToken::new()))
    }
}

/// 主进程日志（任务 7a）：以前只写 stderr，Windows 去掉控制台后（见 `windows_subsystem`
/// 属性）这些日志会彻底丢失，无法排障。现在固定再写一份到 `{LOOKLOOK_HOME}/logs/looklook.log`
/// （不按大小/日期滚动，简单起见；文件会一直增长，需要的话手动清理）；stderr 那份继续保留，
/// 有终端时（含 Windows 接回控制台之后）照样能看到。
///
/// 用 `std::sync::Once` 包一层：托盘“停止再启动”会再跑一次 `run()`，但全局日志订阅者
/// 只能设置一次，重复调用 `tracing_subscriber::...::init()` 会 panic。
fn init_tracing(paths: &Paths) {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::EnvFilter;

        let filter = || EnvFilter::try_from_env("LOOKLOOK_LOG").unwrap_or_else(|_| EnvFilter::new("info,rathole=warn,hyper=warn"));

        let file_appender = tracing_appender::rolling::never(paths.home.join("logs"), "looklook.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
        // non-blocking writer 靠这个 guard 的 Drop 触发最后一次落盘；进程活多久这个日志就要
        // 活多久，泄漏掉（`Box::leak`）比在某个非退出路径上不小心把它提前 drop 掉更安全。
        Box::leak(Box::new(guard));

        use tracing_subscriber::Layer;

        // 日志过滤只作用在两个输出层上；通道状态层要看到 rathole 的 info 事件（“Control channel
        // established”），所以它有自己的过滤，不受上面 rathole=warn 的限制。
        let file_layer = tracing_subscriber::fmt::layer().with_ansi(false).with_target(false).with_writer(file_writer).with_filter(filter());
        let stderr_layer = tracing_subscriber::fmt::layer().with_target(false).with_writer(std::io::stderr).with_filter(filter());
        let relay_layer = relay::StatusLayer.with_filter(EnvFilter::new("rathole=info"));
        // 错误记录（设置 → 问题与日志）：只要 WARN 以上，第三方库里过于啰嗦的连接错误不要
        diag::init(&paths.home.join("logs"));
        diag::install_panic_hook();
        let error_layer = diag::ErrorLayer.with_filter(EnvFilter::new("warn,hyper=error,hyper_util=error,rustls=error,tungstenite=error"));

        tracing_subscriber::registry().with(file_layer).with(stderr_layer).with(relay_layer).with(error_layer).init();
    });
}

async fn run(home: Option<PathBuf>, listen: Option<SocketAddr>, server: Option<String>, no_browser: bool, shutdown: CancellationToken) -> Result<()> {
    let mut paths = Paths::discover(home)?;
    init_tracing(&paths);
    // 看看装的 Node.js、npm 全局目录、Claude Code 原生安装的目录加进 PATH（tools.rs）
    tools::init(&paths.home);
    // 更新后由旧版本拉起：等旧进程退出，不再打开浏览器（管理台页面会自己刷新）
    let restarted = std::env::var_os(updater::RESTART_ENV).is_some();
    if restarted {
        std::env::remove_var(updater::RESTART_ENV);
    }
    let no_browser = no_browser || restarted;
    hardening::probe_debugger();
    let entry = versions::entry().map(|p| p.display().to_string());
    tracing::info!(home = %paths.home.display(), version = account::VERSION, target = paths::target(), exe = ?updater::exe_path(), entry, "看看客户端启动");
    if paths.ttyd.is_none() {
        tracing::warn!("没有找到 ttyd，终端无法启动；请使用完整安装包，或设置 TTYD_BIN");
    }
    if paths.fonts.is_none() {
        tracing::warn!("没有找到终端字体，将使用系统字体");
    }
    // 同一个数据目录只能跑一份：端口不再固定（被占用会自动换），不能再靠“端口被占用”判断已经在运行。
    // 锁随本次 run() 结束释放（托盘“停止再启动”在同一进程里重新加锁）。
    let mut locked = lock_instance(&paths);
    if restarted {
        for _ in 0..60 {
            if locked.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            locked = lock_instance(&paths);
        }
    }
    let _instance_lock = match locked {
        Ok(lock) => lock,
        Err(()) => {
            // 已经在运行（例如双击了两次）：等它就绪后直接打开管理台。
            let home_opt = Some(paths.home.clone());
            let fallback = listen.unwrap_or(listen::DEFAULT);
            for _ in 0..30 {
                if already_running(&home_opt, fallback).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            println!("看看已经在运行。");
            if !no_browser && util::has_desktop() {
                open_ui(&local_url(&home_opt, fallback)?)?;
            }
            return Ok(());
        }
    };
    let store = Arc::new(Store::open(&paths.db())?);
    {
        let p = pets::PetSettings::load(&store);
        desk_pet::set_wanted(p.enabled && p.desktop);
        desk_pet::set_through(p.through);
    }
    tokio::spawn(desk_pet::apply_toggles(store.clone()));
    let ui = listen::bind(listen, &store).await?;
    hardening::check_self_integrity(&store);
    let account = Account::new(store.clone(), paths.clone(), &hardening::default_server(), &hardening::builtin_platform_keys())?;
    if let Some(url) = server {
        if account.platform().base != url.trim_end_matches('/') {
            match account.set_server(&url) {
                Ok(()) => tracing::info!(server = %url, "已切换看看服务端"),
                Err(e) => tracing::warn!(error = %e, "无法切换服务端"),
            }
        }
    }
    updater::rescue_mux_backups(); // 要在第一次调用会话保持程序之前
    updater::settle_trzsz();
    paths.trzsz = paths::find_trzsz().map(|d| versions::stable_trzsz(&paths.home, d));
    if let Err(e) = settings::Settings::import_synced(&store) {
        tracing::warn!(error = %e, "设置同步：导入现有设置失败");
    }
    let instances = Instances::new(store.clone(), paths.clone());
    let updater = updater::Updater::new(store.clone(), paths.clone(), account.clone(), shutdown.clone());
    let metrics = metrics::Metrics::new();
    let relay_watch = relay_watch::RelayWatch::new(account.clone());
    let transfers = transfer::Transfers::new(paths.clone());
    let chat = chat::Chat::new(store.clone(), paths.clone(), instances.clone());
    chat.watch_stops();
    instances.recover().await;
    let relay = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    // 监听 0.0.0.0 时，本机命令行和浏览器仍然用 127.0.0.1 连。
    let ui_addr = match ui.local_addr()? {
        a if a.ip().is_unspecified() => SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port()),
        a => a,
    };
    let relay_addr = relay.local_addr()?;
    rtc::net::set_current(rtc::net::NetConfig::load(&store));
    rtc::nat::load(&store);
    rtc::outbound::load_direct_only(&store);
    let access = gateway::access::AccessConfig::load(&store);
    // 首次运行时生成的访问码要存下来，否则每次启动都不一样。
    access.save(&store)?;
    let _ = std::fs::remove_file(paths.home.join("local-token")); // 旧版本的本机访问口令，已不再使用
    let app = Arc::new(Inner {
        direct: direct_tls::Direct::new(store.clone(), ui_addr.port()),
        shared: Arc::new(shared_pcs::SharedPcs::new(store.clone())),
        account: account.clone(),
        instances: instances.clone(),
        updater: updater.clone(),
        metrics: metrics.clone(),
        relay_watch: relay_watch.clone(),
        transfers: transfers.clone(),
        chat: chat.clone(),
        store: store.clone(),
        paths: paths.clone(),
        ui_addr,
        relay_addr,
        http: gateway::proxy_client(),
        access: std::sync::RwLock::new(access),
        limiter: Default::default(),
        resolved: Default::default(),
        lan_ips: gateway::access::own_lan_ips(),
        remote: Default::default(),
        rtc: Default::default(),
        outbound: Default::default(),
        ssh_transfer: Default::default(),
        sync: Default::default(),
        notify: Default::default(),
        plugins: plugins::Plugins::new(paths.clone(), account.clone(), &hardening::builtin_plugin_keys()),
    });
    app.plugins.set_host(ui_addr);
    std::fs::write(paths.home.join("run").join("endpoint"), ui_addr.to_string())?;
    if let Err(e) = chat.publish(ui_addr) {
        tracing::warn!(error = %e, "无法写入对话界面的 hook 配置");
    }
    let open_base = app.write_open_file()?;

    // `shutdown` 现在是调用者传进来的（不再在这里创建）：托盘的“停止”菜单（任务 7b，
    // 见 src/desktop.rs）靠取消同一个 token 来关掉这次 `run()`，不用等 Ctrl+C/SIGTERM。
    // 这些后台任务都必须随本次 run() 结束（见下方收尾）：托盘“停止→启动”在同一进程里重来，
    // 留下的旧任务会和新的并存——两份 rathole 用同一令牌互相挤掉、两份心跳、两份终端守护。
    let mut relay_task = tokio::spawn(relay::run(account.clone(), relay_addr, paths.relay_config(), shutdown.clone()));
    let background = [
        tokio::spawn(account.clone().heartbeat_loop()),
        tokio::spawn(account.clone().report_terminals_loop(instances.subscribe())),
        tokio::spawn(instances.clone().supervise()),
        tokio::spawn(updater.clone().run()),
        tokio::spawn(updater.clone().keep_versions()),
        tokio::spawn(metrics.run(instances.clone(), chat.clone())),
        tokio::spawn(relay_watch.run(shutdown.clone())),
        tokio::spawn(transfers.run(store.clone())),
        tokio::spawn(app.rtc.clone().watch(account.clone())),
        tokio::spawn(app.outbound.clone().watch(account.clone())),
        tokio::spawn(rtc::signal::run(app.clone(), shutdown.clone())),
        tokio::spawn(app.sync.clone().run(app.clone(), shutdown.clone())),
        tokio::spawn(chat.clone().background()),
        tokio::spawn(gateway::assistant::run(app.clone())),
        tokio::spawn(idle_stop::run(store.clone(), instances.clone(), chat.clone())),
        tokio::spawn(agent_update::run(store.clone(), instances.clone(), chat.clone())),
        tokio::spawn(pricing::run(paths.home.clone())),
        tokio::spawn(rtc::net::resolve_loop()),
        tokio::spawn(rtc::nat::startup(account.clone(), store.clone())),
        tokio::spawn(app.direct.clone().run(account.clone())),
        tokio::spawn({
            usage_stats::preload(&paths.home);
            usage_stats::run(paths.home.clone())
        }),
        tokio::spawn(app.notify.clone().run(app.clone(), shutdown.clone())),
        // libdatachannel 默认第一条连接时才初始化（usrsctp、OpenSSL、线程池），提前做掉，第一次建连少等一会儿
        tokio::task::spawn_blocking(datachannel::preload),
        {
            let app = app.clone();
            tokio::spawn(app.resolved.clone().run(move || app.access()))
        },
        {
            let account = account.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    account.persist_clock();
                }
            })
        },
    ];
    let local = tokio::spawn(gateway::serve_console(ui, gateway::local_app(app.clone()), app.direct.clone(), shutdown.clone()));
    // 插件清单说要随看看起来的（例如上次退出时浏览器还开着）：网关起来以后再起，插件一起来就能调宿主接口
    tokio::spawn({
        let plugins = app.plugins.clone();
        async move { plugins.autostart().await }
    });
    let remote = tokio::spawn(gateway::serve(relay, gateway::relay_app(app.clone()), shutdown.clone()));

    let url = format!("{open_base}/");
    *UI_URL.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("http://{ui_addr}/"));
    println!("看看客户端已启动：{url}");
    let cfg = app.access();
    match (cfg.allow_lan, app.lan_ips.first()) {
        (true, Some(ip)) => println!("局域网里的设备可以打开：http://{ip}:{}/{}", ui_addr.port(), if cfg.code_enabled { "（需要访问码）" } else { "" }),
        _ => println!("只允许这台电脑自己打开管理台；要让局域网里的设备打开，运行 looklook access lan on"),
    }
    println!("如果这台机器有公网 IP，请不要把端口 {} 开放到公网。", ui_addr.port());
    if !no_browser && util::has_desktop() {
        if let Err(e) = open_ui(&url) {
            tracing::warn!(error = %e, "无法自动打开浏览器，请运行 looklook open");
        }
    }

    // 等系统信号（Ctrl+C/SIGTERM）或者外部（托盘“停止”/“退出”）取消 `shutdown`，谁先到算谁。
    tokio::select! {
        _ = wait_for_signal() => {}
        _ = shutdown.cancelled() => {}
    }
    tracing::info!("正在退出…（终端里的任务会继续运行）");
    UI_URL.lock().unwrap_or_else(|e| e.into_inner()).take();
    shutdown.cancel(); // 上面两条路径都可能先到；再调一次是幂等的，确保下面用到 shutdown.clone() 的任务一定会收到取消
    for task in &background {
        task.abort();
    }
    // 助理跟着客户端一起关：Codex 的 app-server 是共用的、客户端退出后还在，不放下的话助理在里面接着跑，
    // 下次启动重启不了它。Claude Code stream-json 进程也在这里结束，不等管道断开。
    if let Some(brain) = assistant::brain_id(&app.store) {
        if tokio::time::timeout(Duration::from_secs(5), app.chat.release(&brain)).await.is_err() {
            tracing::warn!(brain, "退出：助理没在 5 秒内停下");
        }
    }
    instances.shutdown().await;
    // 插件进程跟着看看退出（插件自己的子进程，例如还开着的浏览器，由插件决定留不留，下次启动时接回来）
    app.plugins.stop_all().await;
    account.persist_clock();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        let _ = local.await;
        let _ = remote.await;
    })
    .await;
    // 关远程通道（rathole 自己最多等 5 秒，超时就 abort）。
    if tokio::time::timeout(Duration::from_secs(6), &mut relay_task).await.is_err() {
        relay_task.abort();
    }
    if updater::restart_requested() {
        drop(_instance_lock);
        updater::restart_now();
    }
    Ok(())
}

/// 这次 run() 的本机管理台地址（桌面应用窗口加载它；没在运行时为空）。
/// 窗口始终用本机地址：“打开地址”设置是给浏览器和局域网用的。
static UI_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn ui_url() -> Option<String> {
    UI_URL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("signal");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn open_ui(url: &str) -> Result<()> {
    if util::open_url(url) {
        Ok(())
    } else {
        bail!("无法打开浏览器，请手动访问：{url}")
    }
}

/// 正在运行的客户端的本机地址（本机访问不需要访问码）。
fn endpoint(home: &Option<PathBuf>, listen: SocketAddr) -> Result<String> {
    let paths = Paths::discover(home.clone())?;
    let fallback = if listen.ip().is_unspecified() { SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), listen.port()) } else { listen };
    Ok(std::fs::read_to_string(paths.home.join("run").join("endpoint")).map(|s| s.trim().to_string()).unwrap_or_else(|_| fallback.to_string()))
}

/// 浏览器打开的地址：设置了打开地址时用它（写在 `run/open`），否则用本机地址。
fn local_url(home: &Option<PathBuf>, listen: SocketAddr) -> Result<String> {
    let addr = endpoint(home, listen)?;
    let paths = Paths::discover(home.clone())?;
    let base = std::fs::read_to_string(paths.home.join("run").join("open")).map(|s| s.trim().to_string()).unwrap_or_else(|_| format!("http://{addr}"));
    Ok(format!("{base}/"))
}

/// 数据目录的单实例锁（`run/lock`）。`Err` 表示另一份已经持有；文件系统不支持加锁时不拦。
fn lock_instance(paths: &Paths) -> std::result::Result<Option<std::fs::File>, ()> {
    let path = paths.home.join("run").join("lock");
    let file = match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "无法创建实例锁");
            return Ok(None);
        }
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(()),
        Err(std::fs::TryLockError::Error(e)) => {
            tracing::warn!(error = %e, "无法加实例锁");
            Ok(None)
        }
    }
}

/// 上次写下的地址上是不是正在运行的看看（短超时：那个端口上可能是别的、不应答的程序）。
async fn already_running(home: &Option<PathBuf>, listen: SocketAddr) -> bool {
    let Ok(addr) = endpoint(home, listen) else { return false };
    let Ok(c) = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build() else { return false };
    let Ok(resp) = c.get(format!("http://{addr}/api/status")).header(gateway::CLIENT_HEADER, "1").send().await else { return false };
    resp.status().is_success() && resp.json::<Value>().await.is_ok_and(|v| v["capabilities"].is_object())
}

/// 命令行子命令通过本机接口操作正在运行的客户端。
async fn call(home: &Option<PathBuf>, listen: SocketAddr, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let addr = endpoint(home, listen)?;
    let c = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(60)).build()?;
    let mut req = c.request(method.parse()?, format!("http://{addr}{path}")).header(gateway::CLIENT_HEADER, "1");
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.context(NOT_RUNNING)?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = v["error"]["code"].as_str().unwrap_or("INTERNAL");
        bail!("{}", cli_error(code, &v["error"]["params"]));
    }
    Ok(v)
}

const NOT_RUNNING: &str = "连不上看看客户端，请先启动它：looklook run（安装为服务时：systemctl --user start looklook）\n\
Can't reach the Looklook client. Start it with: looklook run (installed as a service: systemctl --user start looklook)";

/// 授权登录：打印授权码、网址和二维码，等用户在任意设备（通常是手机）的浏览器里批准。
/// 没有图形界面的服务器就靠它登录；Ctrl+C 取消。
async fn cli_login(home: &Option<PathBuf>, listen: SocketAddr, key: Option<String>) -> Result<()> {
    let st = call(home, listen, "GET", "/api/status", None).await?;
    if st["account"]["logged_in"].as_bool() == Some(true) {
        print_status(&st["account"]);
        println!("要换账户，先运行 looklook logout / To switch accounts, run looklook logout first");
        return Ok(());
    }
    let key = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    if let Some(k) = &key {
        // 安装密钥：平台直接批准，不用扫码、不开浏览器（云服务器、开机脚本）
        if !k.starts_with("LLK-") {
            bail!("安装密钥是 LLK- 开头的 / Install keys start with LLK-");
        }
        call(home, listen, "POST", "/api/auth/login", Some(json!({ "install_key": k }))).await?;
        println!("用安装密钥登录… / Signing in with the install key…");
    } else {
        let v = call(home, listen, "POST", "/api/auth/login", Some(json!({}))).await?;
        let url = v["verification_url"].as_str().unwrap_or("-");
        println!("用手机扫描二维码，或在任意设备的浏览器中打开下面的网址，核对授权码后批准这台电脑：");
        println!("Scan the QR code with your phone, or open the link in any browser, check the code and approve this computer:");
        if let Some(qr) = qr_text(url) {
            println!("\n{qr}");
        }
        println!("  网址 / Link: {url}");
        println!("  授权码 / Code: {}", v["user_code"].as_str().unwrap_or("-"));
        let minutes = v["expires_in"].as_u64().map(|s| s.div_ceil(60)).unwrap_or(10);
        println!("\n等待批准…（{minutes} 分钟内有效，Ctrl+C 取消）/ Waiting for approval… (valid for {minutes} min, Ctrl+C to cancel)");
        if util::has_desktop() {
            util::open_url(url);
        }
    }
    let wait = async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let s = call(home, listen, "GET", "/api/auth/status", None).await?;
            match s["state"].as_str() {
                Some("approved") => return Ok(()),
                Some("failed") => {
                    let code = s["error"]["code"].as_str().unwrap_or("INTERNAL");
                    let mut msg = cli_error(code, &s["error"]["params"]);
                    if code == "DEVICE_LIMIT_REACHED" {
                        if let Ok(st) = call(home, listen, "GET", "/api/status", None).await {
                            if let Some(server) = st["account"]["server"].as_str() {
                                msg.push_str(&format!("\n  {}/devices", server.trim_end_matches('/')));
                            }
                        }
                    }
                    bail!("{msg}")
                }
                Some("idle") => bail!("授权已取消 / Sign-in was cancelled"),
                _ => {}
            }
        }
    };
    tokio::select! {
        r = wait => r?,
        _ = tokio::signal::ctrl_c() => {
            let _ = call(home, listen, "POST", "/api/auth/login/cancel", Some(json!({}))).await;
            bail!("授权已取消 / Sign-in was cancelled")
        }
    }
    let v = call(home, listen, "GET", "/api/status", None).await?;
    println!("✓ 登录成功 / Signed in");
    print_status(&v["account"]);
    Ok(())
}

/// 网址的终端二维码（两行像素拼一个字符）。颜色反过来画：深色背景的终端最常见，
/// 这样扫出来是白底黑码，和 `qrencode -t UTF8` 一样。
fn qr_text(url: &str) -> Option<String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::with_error_correction_level(url, qrcode::EcLevel::L).ok()?;
    Some(code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build())
}

/// `looklook access`：查看或修改访问控制，然后打印当前状态。
async fn cli_access(home: &Option<PathBuf>, listen: SocketAddr, action: Option<AccessCmd>) -> Result<()> {
    if let Some(AccessCmd::Port { value }) = action {
        let port: Option<u16> = if value.eq_ignore_ascii_case("auto") || value == "0" {
            None
        } else {
            let p: u16 = value.parse().ok().filter(|n| *n > 0).context("端口必须是 1–65535 之间的数字，或 auto / Port must be 1–65535, or `auto`")?;
            Some(p)
        };
        let body = json!({ "custom_port": port });
        match call(home, listen, "PUT", "/api/connection", Some(body)).await {
            Ok(v) => {
                let cur = v["port"].as_u64().unwrap_or(0);
                match port {
                    Some(p) if p as u64 == cur => println!("监听端口已固定为 {p} / Listening port fixed to {p}"),
                    Some(p) => println!("已设置监听端口为 {p}（当前仍运行在 {cur}），运行 looklook restart 生效 / Listening port set to {p} (currently {cur}); run `looklook restart` to apply"),
                    None => println!("已恢复自动分配端口（当前运行在 {cur}），重启后生效 / Listening port reset to automatic (currently {cur}); takes effect on next restart"),
                }
                return Ok(());
            }
            Err(e) if e.chain().any(|c| c.is::<reqwest::Error>()) => {
                let store = Store::open(&Paths::discover(home.clone())?.db())?;
                listen::set_custom_port(&store, port)?;
                match port {
                    Some(p) => println!("已设置监听端口为 {p}，下次启动生效 / Listening port set to {p}; takes effect on next start"),
                    None => println!("已恢复自动分配端口，下次启动生效 / Listening port reset to automatic; takes effect on next start"),
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    let body = match action {
        None => None,
        Some(AccessCmd::Lan { state, .. }) => Some(json!({ "allow_lan": matches!(state, OnOff::On) })),
        Some(AccessCmd::Code { action: CodeAction::On }) => Some(json!({ "code_enabled": true })),
        Some(AccessCmd::Code { action: CodeAction::Off }) => Some(json!({ "code_enabled": false })),
        Some(AccessCmd::Code { action: CodeAction::New }) => Some(json!({ "new_code": true })),
        Some(AccessCmd::Allow { ip }) => Some(json!({ "add_ip": ip })),
        Some(AccessCmd::Remove { ip }) => Some(json!({ "remove_ip": ip })),
        Some(AccessCmd::Port { .. }) => unreachable!(),
    };
    let v = match body {
        Some(b) => call(home, listen, "PUT", "/api/access", Some(b)).await?,
        None => call(home, listen, "GET", "/api/access", None).await?,
    };
    println!("这台电脑自己 / This computer: 总是可以打开，不需要访问码 / always allowed, no code needed");
    print_access(&v);
    Ok(())
}

/// 局域网访问、白名单、访问码三行（`looklook access` 和 `looklook status` 共用）。
fn print_access(v: &Value) {
    let on = |k: &str| if v[k].as_bool() == Some(true) { "开启 / on" } else { "关闭 / off" };
    match v["lan_ips"][0].as_str() {
        Some(ip) => println!("局域网访问 / LAN access: {}（http://{ip}:{}/）", on("allow_lan"), v["port"]),
        None => println!("局域网访问 / LAN access: {}", on("allow_lan")),
    }
    let ips: Vec<String> = v["allowed_ips"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|x| match v["resolved"][x].as_array() {
            // 域名后面带上它当前解析到的 IP
            Some(r) if r.is_empty() => format!("{x}（解析不到 / unresolved）"),
            Some(r) => format!("{x} → {}", r.iter().filter_map(|i| i.as_str()).collect::<Vec<_>>().join(" ")),
            None => x.to_string(),
        }).collect())
        .unwrap_or_default();
    println!("白名单 / Whitelist: {}", if ips.is_empty() { "（无 / none）".to_string() } else { ips.join(", ") });
    println!("访问码 / Access code: {}{}", on("code_enabled"), if v["code_enabled"].as_bool() == Some(true) { format!(", {}", v["code"].as_str().unwrap_or("")) } else { String::new() });
    if !ips.is_empty() && v["code_enabled"].as_bool() != Some(true) {
        println!("提示：已添加白名单，建议开启访问码 / Tip: with a whitelist, turn on the access code: looklook access code on");
    }
}

/// `looklook rtc`：查看或修改远程连接的直连设置（服务端 docs/07）。客户端在运行就经它的接口改（立即用于新连接），
/// 没在运行就直接改本机数据库。
async fn cli_rtc(home: &Option<PathBuf>, listen: SocketAddr, action: Option<RtcCmd>) -> Result<()> {
    use rtc::net::{NetConfig, NetUpdate};
    let not_running = |e: &anyhow::Error| e.chain().any(|c| c.is::<reqwest::Error>());
    let offline = || -> Result<Store> { Ok(Store::open(&Paths::discover(home.clone())?.db())?) };
    let off_or = |a: String| if a.eq_ignore_ascii_case("off") { String::new() } else { a };
    let update = match action {
        None => None,
        Some(RtcCmd::Ports { range }) => {
            let (b, e) = if range.eq_ignore_ascii_case("default") {
                rtc::net::DEFAULT_PORTS
            } else {
                range
                    .split_once('-')
                    .and_then(|(b, e)| Some((b.trim().parse().ok()?, e.trim().parse().ok()?)))
                    .context("端口范围写成 起始-结束，例如 50000-50031 / Write the range as begin-end, e.g. 50000-50031")?
            };
            Some(NetUpdate { port_begin: Some(b), port_end: Some(e), ..Default::default() })
        }
        Some(RtcCmd::Ipv4 { addr }) => Some(NetUpdate { public_v4: Some(off_or(addr)), ..Default::default() }),
        Some(RtcCmd::Ipv6 { addr }) => Some(NetUpdate { public_v6: Some(off_or(addr)), ..Default::default() }),
        Some(RtcCmd::Ipv6Enabled { state }) => Some(NetUpdate { ipv6: Some(matches!(state, OnOff::On)), ..Default::default() }),
        Some(RtcCmd::Check) => {
            let d = call(home, listen, "POST", "/api/rtc/net/detect", Some(json!({})))
                .await
                .map_err(|e| if not_running(&e) { anyhow::anyhow!("客户端没在运行，先启动看看 / The client isn't running; start Looklook first") } else { e })?;
            print_nat(&d["nat"]);
            // 退出码 0 = 可以直连（open），方便安装脚本检查（服务端 docs/08 §7）
            if d["nat"]["verdict"].as_str() != Some("open") {
                std::process::exit(2);
            }
            return Ok(());
        }
        Some(RtcCmd::Detect { apply }) => {
            let d = match call(home, listen, "POST", "/api/rtc/net/detect", Some(json!({}))).await {
                Ok(v) => v,
                Err(e) if not_running(&e) => serde_json::to_value(rtc::net::detect(None).await)?,
                Err(e) => return Err(e),
            };
            print_detected(&d);
            if !d["nat"].is_null() {
                print_nat(&d["nat"]);
            }
            if !apply {
                if d["suggest_v4"].is_string() || d["suggest_v6"].is_string() {
                    println!("加 --apply 保存 / Add --apply to save");
                }
                return Ok(());
            }
            let pick = |k: &str| d[k].as_str().map(str::to_string);
            let u = NetUpdate { public_v4: pick("suggest_v4"), public_v6: pick("suggest_v6"), ..Default::default() };
            if u.public_v4.is_none() && u.public_v6.is_none() {
                bail!("没有检测到地址，没有保存 / Nothing detected; nothing saved");
            }
            Some(u)
        }
    };
    let v = match &update {
        Some(u) => call(home, listen, "PUT", "/api/rtc/net", Some(serde_json::to_value(u)?)).await,
        None => call(home, listen, "GET", "/api/rtc/net", None).await,
    };
    let v = match v {
        Ok(v) => v,
        Err(e) if not_running(&e) => {
            let store = offline()?;
            let mut c = NetConfig::load(&store);
            if let Some(u) = &update {
                c = c.updated(u).map_err(|(_, rule)| anyhow::anyhow!(cli_error("VALIDATION_FAILED", &json!({ "rule": rule }))))?;
                c.save(&store)?;
                println!("客户端没在运行，设置已保存，下次启动生效 / The client isn't running; saved for its next start");
            }
            rtc::net::resolve(&c).await;
            rtc::net::view(&c)
        }
        Err(e) => return Err(e),
    };
    print_rtc(&v);
    Ok(())
}

/// `looklook https`（服务端 docs/09 §7）：开发机安装脚本 `--direct` 也用它。
async fn cli_https(home: &Option<PathBuf>, listen: SocketAddr, action: Option<HttpsCmd>) -> Result<()> {
    let wait = match action {
        None => false,
        Some(HttpsCmd::Off) => {
            call(home, listen, "PUT", "/api/public-https", Some(json!({ "enabled": false }))).await?;
            println!("公网 HTTPS 直连已关闭 / Public HTTPS is off");
            return Ok(());
        }
        Some(HttpsCmd::On { wait }) => {
            let net = call(home, listen, "GET", "/api/rtc/net", None).await?;
            if net["public_v4"].as_str().unwrap_or("").is_empty() {
                // 用户明确要开：检测到就填上（设置页里只提示、不自动填）
                let d = call(home, listen, "POST", "/api/rtc/net/detect", Some(json!({}))).await?;
                match d["suggest_v4"].as_str() {
                    Some(ip) => {
                        call(home, listen, "PUT", "/api/rtc/net", Some(json!({ "public_v4": ip }))).await?;
                        println!("检测到公网 IPv4 / Detected public IPv4: {ip}");
                    }
                    None => println!("没检测到公网 IPv4，用 looklook rtc ipv4 <地址> 填写 / No public IPv4 detected; set it with looklook rtc ipv4 <addr>"),
                }
            }
            call(home, listen, "PUT", "/api/public-https", Some(json!({ "enabled": true }))).await?;
            wait
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let v = loop {
        let v = call(home, listen, "GET", "/api/public-https", None).await?;
        let phase = v["status"]["phase"].as_str().unwrap_or("");
        let failed_once = v["status"]["error"].is_string();
        if !wait || phase == "ok" || phase == "off" || phase == "no_ip" || failed_once || std::time::Instant::now() > deadline {
            break v;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    print_https(&v);
    Ok(())
}

fn print_https(v: &Value) {
    let st = &v["status"];
    let s = |k: &str| st[k].as_str().unwrap_or("");
    let port = v["port"].as_u64().unwrap_or(0);
    if v["enabled"] != json!(true) {
        println!("公网 HTTPS 直连：关闭 / Public HTTPS: off（looklook https on）");
        return;
    }
    match s("phase") {
        "ok" => {
            println!("公网 HTTPS 直连 / Public HTTPS: {}/", s("url"));
            if let Some(t) = st["not_after"].as_i64().and_then(|t| time::OffsetDateTime::from_unix_timestamp(t).ok()) {
                println!("证书到期 / Certificate expires: {}（自动续期 / renewed automatically）", t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default());
            }
            if !s("reach").is_empty() && s("reach") != "ok" && s("reach") != "unknown" {
                println!("⚠ 看看服务器从外面打不开（{}），请在安全组里放行 TCP {port} / Not reachable from outside; allow TCP {port}", s("reach"));
            }
        }
        "no_ip" => println!("公网 HTTPS 直连：还不知道公网 IPv4，用 looklook rtc ipv4 <地址> 填写 / Public IPv4 unknown; set it with looklook rtc ipv4 <addr>"),
        "working" => println!("公网 HTTPS 直连：正在申请证书 / Requesting a certificate…"),
        _ => println!("公网 HTTPS 直连：申请证书失败 / Requesting a certificate failed"),
    }
    if !s("error").is_empty() {
        println!("  原因 / Reason: {}{}", s("error"), if s("detail").is_empty() { String::new() } else { format!("（{}）", s("detail")) });
        if matches!(s("error"), "port_busy" | "no_privilege") {
            println!("  80、443 不能用时，在 nginx 里转发验证路径 / If 80 and 443 aren't available, forward the validation path in nginx:");
            println!("    location /.well-known/acme-challenge/ {{ proxy_pass http://127.0.0.1:{port}; }}");
        }
    }
    if v["code_ok"] == json!(true) {
        if v["logged_in"] != json!(true) {
            println!("访问码 / Access code: {}", v["code"].as_str().unwrap_or(""));
        }
    } else {
        println!("⚠ 访问码不到 12 位，不能用于公网打开：looklook access code new / The access code is too short for internet use: looklook access code new");
    }
    println!("安全组 / 防火墙放行 TCP 80（证书）和 TCP {port}（管理台）/ Allow TCP 80 (certificates) and TCP {port} (console) in the firewall");
    if let Some(ip) = st["ip"].as_str() {
        println!("Clash 加速 / To use your proxy: - IP-CIDR,{ip}/32,<节点 / proxy>,no-resolve   # 放在 GEOIP,CN,DIRECT 之前 / before GEOIP,CN,DIRECT");
    }
}

fn print_rtc(v: &Value) {
    let addr = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).unwrap_or("（未填 / not set）").to_string();
    println!("传输端口 / Ports: UDP {}-{}", v["port_begin"], v["port_end"]);
    // 填的是域名时带上现在的解析结果
    let addr = |k: &str, r: &str| match v.get(r) {
        Some(Value::String(ip)) => format!("{} → {ip}", addr(k)),
        Some(_) => format!("{}（还没解析出地址 / not resolved yet）", addr(k)),
        None => addr(k),
    };
    println!("公网 IPv4 / Public IPv4: {}", addr("public_v4", "resolved_v4"));
    println!("公网 IPv6 / Public IPv6: {}", addr("public_v6", "resolved_v6"));
    println!("使用 IPv6 / Use IPv6: {}", if v["ipv6"].as_bool() == Some(true) { "开启 / on" } else { "关闭 / off" });
    let nic: Vec<&str> = v["nic_v6"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    if !nic.is_empty() {
        println!("网卡上的公网 IPv6（自动使用）/ Public IPv6 on network adapters (used automatically): {}", nic.join(", "));
    }
    println!(
        "直连需要在防火墙、安全组上放行这段 UDP 端口（IPv4 和 IPv6）；路由器端口转发时外部端口要和内部端口相同。只支持完全锥形 NAT。\n\
         Direct connections need this UDP range open in the firewall / security group (IPv4 and IPv6); when forwarding on a router, use the same external and internal ports. Only full-cone NAT is supported."
    );
}

fn print_detected(d: &Value) {
    for (fam, k) in [("IPv4", "v4"), ("IPv6", "v6")] {
        println!("{fam}:");
        for p in d[k].as_array().into_iter().flatten() {
            let source = p["source"].as_str().unwrap_or("-");
            let source = if source == "relay" { "中转 STUN / relay STUN" } else { source };
            match p["ip"].as_str() {
                Some(ip) => println!("  {source:<24} {ip}"),
                None => println!("  {source:<24} -（{}）", p["error"].as_str().unwrap_or("-")),
            }
        }
        let ips: Vec<&str> = d[k].as_array().into_iter().flatten().filter_map(|p| p["ip"].as_str()).collect();
        if ips.iter().any(|ip| *ip != ips[0]) {
            println!("  各来源结果不一致，可能经过了代理或 VPN / Sources disagree; a proxy or VPN may be in the way");
        }
    }
    let s = |k: &str| d[k].as_str().unwrap_or("-").to_string();
    println!("建议 / Suggested: IPv4 {}, IPv6 {}", s("suggest_v4"), s("suggest_v6"));
}

/// NAT 类型检测的结论（docs/08 §5）。
fn print_nat(r: &Value) {
    let s = |k: &str| r[k].as_str().unwrap_or("");
    let verdict = match s("verdict") {
        "open" => "可以直连：从外面能打进这台电脑的端口段，任何网络下的手机、浏览器都能直接连到它
                   Direct: the port range is reachable from outside, so phones and browsers on any network can connect directly",
        "partial" => "部分放行：端口段只有一部分能从外面打进来，把整段 UDP 都放行 / 转发
                      Partly open: only part of the port range is reachable from outside; open or forward the whole UDP range",
        "wrong_addr" if s("why") == "blocked" => "填的公网地址打不进来：和出口一致，但端口段没放行或转发没生效
                                                 The public address you set can't be reached: it matches the exit, but the port range isn't open or forwarded",
        "punch" => "多数能直连：多数网络能打洞直连，对方在严格 NAT 后面时走中转。想全部直连：放行端口段或做端口转发
                    Mostly direct: hole punching works from most networks; peers behind strict NAT go through the relay. For direct everywhere, open or forward the port range",
        "relay_only" => "只能中转：这台电脑所在的网络是对称 NAT，打不了洞，连接会经中转
                         Relay only: this network uses symmetric NAT, so hole punching fails and connections go through the relay",
        "proxy" => "测不准：代理软件（TUN 模式）在接管 UDP，让中转的 STUN 地址和端口段直连后再测
                    Can't tell: a proxy (TUN mode) is taking over UDP; let the relays' STUN addresses and the port range bypass it, then check again",
        "wrong_addr" if s("why") == "unresolved" => "填的公网地址（域名）还没解析出来 / The public address (domain) hasn't resolved yet",
        "wrong_addr" => "填的公网 IPv4 和实际出口不一致：多半是运营商 NAT，或者 IP 变了，填了也连不进来
                         The public IPv4 you set differs from the actual exit address: likely carrier-grade NAT or a changed IP, so it can't be reached",
        _ => match s("why") {
            "not_logged_in" => "测不了：登录后才能检测 / Can't check: log in first",
            "one_relay" => "测不了：需要两台不同 IP 的中转 / Can't check: needs two relays with different IPs",
            _ => "测不了：中转的 STUN 没有回应（UDP 3478 被拦？）/ Can't check: the relays' STUN didn't answer (UDP 3478 blocked?)",
        },
    };
    println!("NAT: {verdict}");
    let port = r["local_port"].as_u64().unwrap_or(0);
    let range = if r["in_range"].as_bool() == Some(true) { "端口段内 / in range" } else { "端口段全被占用，随机端口 / range busy, random port" };
    println!("  本地端口 / Local port: {port}（{range}）");
    match s("a") {
        "both" | "one" | "none" => {
            let target = s("a_target");
            for p in r["ports"].as_array().into_iter().flatten() {
                let ok = if p["open"].as_bool() == Some(true) { "通 / open" } else { "不通 / closed" };
                println!("  从外面 / From outside → {target}:{}  {ok}", p["port"].as_u64().unwrap_or(0));
            }
        }
        _ => {
            let why = match s("a_why") {
                "ports_busy" => "端口段里没有空闲端口 / no free port in the range",
                "no_target" => "没填公网地址，也没测出公网出口 / no public address set or found",
                "unsupported" => "主服务不支持 / the server doesn't support it yet",
                "limited" => "每小时最多 6 次，稍后再测 / at most 6 per hour, try later",
                "not_logged_in" => "没登录 / not logged in",
                _ => "请求主服务失败 / request to the server failed",
            };
            println!("  从外面打进来 / Reachable from outside: 没测 / not checked（{why}）");
        }
    }
    if let Some(ip) = r["suggest"].as_str() {
        println!("  检测到可以直连，可以填入公网 IPv4 / Direct works; you can set the public IPv4: {ip}（looklook rtc detect --apply）");
    }
    for m in r["mappings"].as_array().into_iter().flatten() {
        let relay = m["relay"].as_str().unwrap_or("-");
        match m["mapped"].as_str() {
            Some(a) => println!("  {relay:<12} → {a}"),
            None => println!("  {relay:<12} -（{}）", m["error"].as_str().unwrap_or("-")),
        }
    }
    if let (Some(e), Some(p)) = (r["exit"].as_str(), r["public_v4"].as_str()) {
        if e != p {
            println!("  出口 / Exit {e} ≠ 填的 / set {p}");
        }
    }
}

/// `looklook restart`：让正在运行的客户端重启（和更新后的重启一样，由系统服务或它自己拉起新进程），等新进程起来。
async fn cli_restart(home: &Option<PathBuf>, listen: SocketAddr) -> Result<()> {
    let old = call(home, listen, "POST", "/api/restart", Some(json!({}))).await?;
    let old_pid = old["pid"].as_u64();
    println!("正在重启 / Restarting…");
    // 系统服务在退出 5 秒后才拉起新进程，最多等 60 秒
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Ok(s) = call(home, listen, "GET", "/api/status", None).await else { continue };
        if s["pid"].as_u64().is_some_and(|p| Some(p) != old_pid) {
            println!("已重启 / Restarted: {}", s["version"].as_str().unwrap_or("-"));
            return Ok(());
        }
    }
    bail!("等了 60 秒还没起来，运行 looklook status 查看；装成服务时可以看日志：journalctl --user -u looklook\nNot back after 60 seconds; run looklook status, or check the service log: journalctl --user -u looklook")
}

fn cli_error(code: &str, params: &Value) -> String {
    match code {
        "AUTHORIZATION_EXPIRED" => "授权已过期，请重新运行 looklook login / The code expired; run looklook login again".into(),
        "AUTHORIZATION_DENIED" => "授权被拒绝 / Sign-in was denied".into(),
        "INSTALL_KEY_INVALID" => "安装密钥不对、已过期、已撤销或次数用完了，请在看看网页的“电脑”页重新生成 / The install key is wrong, expired, revoked or used up; make a new one on the Devices page".into(),
        "INSTALL_KEY_UNSUPPORTED" => "这个看看服务器还不支持安装密钥，请改用 looklook login（不带 --key）/ This Looklook server doesn't support install keys yet; use looklook login without --key".into(),
        "DEVICE_ALREADY_LOGGED_IN" => {
            let name = params["device_name"].as_str().unwrap_or("另一台电脑 / another computer");
            format!(
                "这个账户已经在“{name}”上登录。请先在那台电脑上退出，或到看看网页的“登录的电脑”里让它退出登录。\n\
                 This account is already signed in on \"{name}\". Sign out there first, or sign it out under Devices on the Looklook website."
            )
        }
        "DEVICE_LIMIT_REACHED" => device_limit_text(params),
        "LICENSE_CODE_INVALID" => "授权码不对，请整段复制后再试 / The license code is invalid; copy the whole code and try again".into(),
        "LICENSE_CODE_MACHINE" => "这个授权码不是给这台电脑的，请用 looklook fingerprint 的指纹重新申请 / This code is for another computer; request one for the fingerprint from looklook fingerprint".into(),
        "LICENSE_CODE_EXPIRED" => "这个授权码已经过期 / This license code has expired".into(),
        "MEMBERSHIP_EXPIRED" => "会员已到期，请先在看看网页续费 / Membership expired; renew it on the Looklook website".into(),
        "ACCOUNT_DISABLED" => "账户已停用 / This account is disabled".into(),
        "NETWORK" => "连不上看看服务器，请检查网络 / Can't reach the Looklook server; check the network".into(),
        "ALREADY_LOGGED_IN" => "已经登录了 / Already signed in".into(),
        "CLIENT_VERSION_UNSUPPORTED" => "客户端版本太旧，请先运行 looklook upgrade / This client is too old; run looklook upgrade".into(),
        "VALIDATION_FAILED" => match params["rule"].as_str() {
            Some("ip_format") => "格式不对，填 IP、网段或域名，例如 203.0.113.7、203.0.113.0/24、home.example.com / Invalid entry; use an IP, range or domain, e.g. 203.0.113.7, 203.0.113.0/24, home.example.com".into(),
            Some("ip_prefix") => "网段长度不对，IPv4 是 /8 到 /32 / Invalid prefix length; IPv4 takes /8 to /32".into(),
            Some("ip_too_wide") => "范围太大，不能对整个互联网开放 / Range too wide; it can't open to the whole internet".into(),
            Some("ip_loopback") => "这台电脑自己本来就可以打开，不用添加 / This computer is always allowed; no need to add it".into(),
            Some("ip_too_many") => "白名单最多 32 项 / The whitelist holds at most 32 entries".into(),
            Some("rtc_ports") => "端口范围要在 1025–65535 之间，起始不大于结束 / The range must be within 1025–65535, begin ≤ end".into(),
            Some("rtc_ports_span") => "端口范围至少 8 个端口 / The range needs at least 8 ports".into(),
            Some("port") => "端口必须在 1–65535 之间 / Port must be between 1 and 65535".into(),
            Some("port_busy") => "这个端口已被其他程序占用，请换一个 / This port is already in use; pick another".into(),
            Some("port_denied") => "没有权限监听这个端口（例如 1024 以下的特权端口）/ No permission to bind this port (e.g. privileged port below 1024)".into(),
            Some("port_unavailable") => "这个端口当前无法监听 / This port is unavailable".into(),
            Some("ipv4") => "不是 IPv4 地址 / Not an IPv4 address".into(),
            Some("ipv6") => "不是 IPv6 地址 / Not an IPv6 address".into(),
            Some("host_unusable") => "这个地址连不到（回环、链路本地、组播等）/ This address can't be reached (loopback, link-local, multicast…)".into(),
            Some(r) => format!("填写的内容不正确 / Invalid input: {r}"),
            None => "填写的内容不正确 / Invalid input".into(),
        },
        "REMOTE_FORBIDDEN" => "这个操作只能在这台电脑或局域网里进行 / Only allowed from this computer or the LAN".into(),
        "UPDATE_BUSY" => "已经在更新了 / An update is already running".into(),
        "UPDATE_NONE" => "已是最新版本 / Already up to date".into(),
        "UPDATE_CHECKSUM" => "下载的安装包校验不通过，请重试 / The download failed its checksum; try again".into(),
        "UPDATE_DOWNLOAD_FAILED" => "下载安装包失败，请检查网络后重试 / Download failed; check the network and try again".into(),
        "UPDATE_UNSUPPORTED" => "不能在这里自动更新，请到看看网页的“下载”页手动安装 / Can't update automatically here; install from the Download page".into(),
        "UPDATE_FAILED" => "安装没有完成，原来的版本保持不变 / Install didn't finish; the previous version is unchanged".into(),
        other => format!("操作失败 / Failed: {other}"),
    }
}

/// 登录被拒：已经有上限台数的电脑登录了。列出这些电脑，提示到网页上让其中一台退出登录。
fn device_limit_text(params: &Value) -> String {
    let devices = params["devices"].as_array().cloned().unwrap_or_default();
    let max = params["max"].as_i64().map(|n| n.to_string()).unwrap_or_else(|| devices.len().to_string());
    let mut out = format!("这个账户最多同时在 {max} 台电脑上登录，现在已经满了：\nThis account can be signed in on at most {max} computers, and that's full:");
    for d in &devices {
        let name = d["name"].as_str().filter(|s| !s.is_empty()).unwrap_or("未命名电脑 / unnamed");
        let os = d["os"].as_str().filter(|s| !s.is_empty()).map(|s| format!("（{s}）")).unwrap_or_default();
        let state = if d["online"].as_bool() == Some(true) {
            "在线 / online".to_string()
        } else {
            match d["last_seen_at"].as_str() {
                Some(t) => format!("最后在线 / last seen {}", t.replace('T', " ").split('.').next().unwrap_or(t).trim_end_matches('Z')),
                None => "不在线 / offline".to_string(),
            }
        };
        out.push_str(&format!("\n  · {name}{os}  {state}"));
    }
    out.push_str("\n请到看看网页的“我的电脑”里让其中一台退出登录，然后再登录。\nSign one of them out under My computers on the Looklook website, then sign in again.");
    out
}

fn print_status(v: &Value) {
    let v = if v.get("account").is_some() { &v["account"] } else { v };
    if v["logged_in"].as_bool() != Some(true) {
        println!("未登录：这台电脑上和同一 Wi‑Fi 里照常使用 / Not signed in: works on this computer and the same Wi‑Fi（服务端 / server: {}）", v["server"].as_str().unwrap_or("-"));
        println!("开通远程访问，出门在外也能用手机打开：looklook login（新用户注册送一年）");
        println!("Turn on remote access to open it from your phone anywhere: looklook login (first year free)");
        return;
    }
    let s = &v["session"];
    println!("已登录 / Signed in: {}（{}）", s["user"]["nickname"].as_str().unwrap_or(""), s["user"]["username"].as_str().unwrap_or(""));
    println!("专属地址 / Your address: {}", s["user_host_url"].as_str().unwrap_or("-"));
    println!("可以使用到 / Membership until: {}", s["membership_expires_at"].as_str().unwrap_or("-"));
    if v["remote_allowed"].as_bool() == Some(false) {
        println!("远程访问暂停了，这台电脑上照常使用 / Remote access is paused; it keeps working here: {}", v["reason"].as_str().unwrap_or("-"));
        if v["reason"].as_str() == Some("MEMBERSHIP_EXPIRED") {
            println!("延长会员后马上恢复：管理台 → 会员与邀请，兑换续期码或邀请朋友注册 / Extend your membership to resume: console → Membership, redeem a renewal code or invite friends");
        }
    }
}
