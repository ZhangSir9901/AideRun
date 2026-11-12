// AideRun - 只读系统代理探测、常见翻墙端口扫描、Google 连通校验及 config.json 同步模块

use std::collections::HashSet;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
use winreg::RegKey;

use crate::config::{LauncherConfig, ProxyPolicyConfig};
use crate::console::CREATE_NO_WINDOW;
use crate::paths::PortablePaths;

const GOOGLE_PROBE_URL_PRIMARY: &str = "https://www.google.com/generate_204";
const GOOGLE_PROBE_URL_FALLBACK: &str = "https://clients3.google.com/generate_204";

/// 常见 Windows 代理软件本地端口清单（端口, 协议, 常见对应客户端）
pub const COMMON_WIN_PROXY_CANDIDATES: &[(u16, &str, &str)] = &[
    (7890, "http", "Clash / Clash Verge / Mihomo"),
    (7897, "http", "Clash Verge Rev / Mihomo Party"),
    (7891, "socks5", "Clash SOCKS5"),
    (10809, "http", "v2rayN / Nekoray HTTP"),
    (10808, "socks5", "v2rayN / Xray SOCKS5"),
    (1080, "socks5", "Shadowsocks / 通用 SOCKS5"),
    (1081, "http", "Shadowsocks HTTP"),
    (10801, "http", "v2ray / 通用 HTTP"),
    (2080, "socks5", "NekoBox / Sing-box SOCKS5"),
    (2081, "http", "NekoBox / Sing-box HTTP"),
    (12334, "http", "Hiddify HTTP"),
    (2334, "socks5", "Hiddify SOCKS5"),
    (20171, "http", "v2rayA HTTP"),
    (20170, "socks5", "v2rayA SOCKS5"),
    (6152, "http", "Surge HTTP"),
    (6153, "socks5", "Surge SOCKS5"),
    (7892, "http", "Clash 备用 HTTP"),
    (8888, "http", "通用本地 HTTP 代理"),
    (8889, "http", "通用本地 HTTP 代理"),
    (9090, "http", "通用本地 HTTP 代理"),
];

/// 解析后的代理端点描述
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxyEndpoint {
    pub proxy_type: String,
    pub host: String,
    pub port: u16,
}

impl ProxyEndpoint {
    /// 生成 HTTP/HTTPS 代理 URL
    pub fn http_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// 生成 ALL_PROXY URL
    pub fn all_proxy_url(&self) -> String {
        format!("{}://{}:{}", self.proxy_type, self.host, self.port)
    }
}

/// 探测到的本地开放代理端口信息
#[derive(Debug, Clone, Serialize)]
pub struct DetectedLocalPort {
    pub endpoint: ProxyEndpoint,
    pub software: &'static str,
}

/// 启动时 Google 联网及代理探测结果
#[derive(Debug, Clone, Serialize)]
pub struct GoogleProbeReport {
    pub reachable: bool,
    pub endpoint: Option<ProxyEndpoint>,
    pub source_label: String,
    pub running_clients: Vec<String>,
    pub open_local_ports: Vec<DetectedLocalPort>,
    pub failure_reason: String,
}

/// 只读读取 Windows Internet Settings 注册表中的系统代理设置（不写入注册表）
pub fn read_windows_system_proxy() -> Option<ProxyEndpoint> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            KEY_READ,
        )
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    if enabled != 1 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    parse_proxy_string(&server)
}

/// 解析多种格式的 Windows ProxyServer 字符串（如 `http=127.0.0.1:7890;socks=...` 或 `127.0.0.1:10808`）
pub fn parse_proxy_string(raw: &str) -> Option<ProxyEndpoint> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.contains('=') {
        let mut fallback: Option<ProxyEndpoint> = None;
        for part in trimmed.split(';') {
            let item = part.trim();
            let Some((proto_raw, addr_raw)) = item.split_once('=') else {
                continue;
            };
            let proto = proto_raw.trim().to_ascii_lowercase();
            if let Some((host, port)) = parse_host_port(addr_raw.trim()) {
                let proxy_type = if proto.starts_with("socks") {
                    "socks5".to_string()
                } else {
                    "http".to_string()
                };
                let ep = ProxyEndpoint {
                    proxy_type,
                    host,
                    port,
                };
                if proto == "http" {
                    return Some(ep);
                }
                if fallback.is_none() {
                    fallback = Some(ep);
                }
            }
        }
        return fallback;
    }

    let (scheme_hint, addr_part) = if let Some((scheme, rest)) = trimmed.split_once("://") {
        (Some(scheme.trim().to_ascii_lowercase()), rest.trim())
    } else {
        (None, trimmed)
    };

    let (host, port) = parse_host_port(addr_part)?;
    let proxy_type = match scheme_hint.as_deref() {
        Some(s) if s.starts_with("socks") => "socks5".to_string(),
        Some(_) => "http".to_string(),
        None => {
            if port == 10808 || port == 1080 {
                "socks5".to_string()
            } else {
                "http".to_string()
            }
        }
    };
    Some(ProxyEndpoint {
        proxy_type,
        host,
        port,
    })
}

fn parse_host_port(input: &str) -> Option<(String, u16)> {
    let clean = input.trim().trim_end_matches('/');
    if clean.starts_with('[') {
        let end = clean.find("]:")?;
        let host = clean[1..end].trim().to_string();
        let port = clean[end + 2..].trim().parse::<u16>().ok()?;
        return Some((host, port));
    }
    let (host, port_str) = clean.rsplit_once(':')?;
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let port = port_str.trim().parse::<u16>().ok()?;
    Some((host.to_string(), port))
}

/// 从代理配置 json 文件（Antigravity-Proxy config.json）读取 proxy 节点（兼容 UTF-8 BOM）
pub fn read_config_json_proxy(config_path: &Path) -> Option<ProxyEndpoint> {
    let raw = std::fs::read_to_string(config_path).ok()?;
    let content = raw.trim_start_matches('\u{FEFF}');
    let root: Value = serde_json::from_str(content).ok()?;
    let proxy = root.get("proxy")?.as_object()?;
    let host = proxy.get("host")?.as_str()?.trim().to_string();
    let port = proxy.get("port")?.as_u64()? as u16;
    let proxy_type = proxy
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("http")
        .trim()
        .to_string();
    if host.is_empty() || port == 0 {
        return None;
    }
    Some(ProxyEndpoint {
        proxy_type,
        host,
        port,
    })
}

/// 从当前进程环境变量中读取已配置的代理（HTTPS_PROXY / HTTP_PROXY / ALL_PROXY）
pub fn read_env_proxy() -> Option<ProxyEndpoint> {
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(val) = std::env::var(key) {
            if let Some(ep) = parse_proxy_string(&val) {
                return Some(ep);
            }
        }
    }
    None
}

/// 根据策略解析有效代理，并在需要时使用标准 JSON 安全更新 config.json
pub fn resolve_and_sync_proxy(
    config_path: &Path,
    policy: &ProxyPolicyConfig,
) -> Option<ProxyEndpoint> {
    let manual_ep = policy
        .manual_proxy
        .as_deref()
        .and_then(parse_proxy_string);

    if policy.lock_manual {
        if let Some(ep) = manual_ep {
            if config_path.exists() {
                let _ = update_config_json_proxy(config_path, &ep);
            }
            return Some(ep);
        }
    }

    let mut current_cfg = read_config_json_proxy(config_path);

    if policy.sync_system_proxy && !policy.lock_manual {
        if let Some(sys) = read_windows_system_proxy() {
            let needs_update = match &current_cfg {
                Some(existing) => existing.host != sys.host || existing.port != sys.port,
                None => false,
            };
            if needs_update && config_path.exists() {
                if update_config_json_proxy(config_path, &sys).is_ok() {
                    current_cfg = Some(sys.clone());
                }
            }
            if current_cfg.is_none() {
                current_cfg = Some(sys);
            }
        }
    }

    current_cfg.or(manual_ep).or_else(read_env_proxy)
}

/// 安全改写 config.json 中的 proxy 字段（兼容 UTF-8 BOM，保留其他全部配置项并原子写回）
pub fn update_config_json_proxy(config_path: &Path, ep: &ProxyEndpoint) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(config_path)?;
    let clean = raw.trim_start_matches('\u{FEFF}');
    let mut root: Value = serde_json::from_str(clean)?;
    if let Some(obj) = root.as_object_mut() {
        obj.insert(
            "proxy".to_string(),
            serde_json::json!({
                "type": ep.proxy_type,
                "host": ep.host,
                "port": ep.port
            }),
        );
    }
    let formatted = serde_json::to_string_pretty(&root)?;
    let tmp_path = config_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, formatted)?;
    std::fs::rename(&tmp_path, config_path)?;
    Ok(())
}

/// 测试代理端口在短超时内是否可建立 TCP 连接（用于 doctor 诊断）
pub fn check_proxy_reachable(ep: &ProxyEndpoint) -> bool {
    check_proxy_reachable_timeout(ep, Duration::from_millis(800))
}

/// 测试代理端口在指定超时内是否可建立 TCP 连接
pub fn check_proxy_reachable_timeout(ep: &ProxyEndpoint, timeout: Duration) -> bool {
    let target = format!("{}:{}", ep.host, ep.port);
    let Ok(addrs) = target.to_socket_addrs() else {
        return false;
    };
    let addrs: Vec<SocketAddr> = addrs.collect();
    for addr in addrs {
        if TcpStream::connect_timeout(&addr, timeout).is_ok() {
            return true;
        }
    }
    false
}

/// 探测当前正在运行的常见 Windows 代理软件客户端名称（仅用于提示展示）
pub fn detect_running_proxy_clients() -> Vec<String> {
    const SIGNATURES: &[(&str, &str)] = &[
        ("clash-verge", "Clash Verge"),
        ("clash verge", "Clash Verge"),
        ("clash-nyanpasu", "Clash Nyanpasu"),
        ("clash", "Clash"),
        ("mihomo", "Mihomo"),
        ("v2rayn", "v2rayN"),
        ("v2raya", "v2rayA"),
        ("nekobox", "NekoBox"),
        ("nekoray", "Nekoray"),
        ("hiddify", "Hiddify"),
        ("shadowsocks", "Shadowsocks"),
        ("sing-box", "sing-box"),
        ("flclash", "FlClash"),
        ("karing", "Karing"),
        ("xray", "Xray"),
    ];

    let mut found = Vec::<String>::new();
    let mut seen = HashSet::<String>::new();

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return found;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
                for &(needle, label) in SIGNATURES {
                    if name.contains(needle) && seen.insert(label.to_string()) {
                        found.push(label.to_string());
                        break;
                    }
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    found
}

/// 并发快速扫描本机常见 Windows 翻墙软件代理端口（127.0.0.1）
pub fn scan_local_proxy_ports() -> Vec<DetectedLocalPort> {
    let hits: Vec<Option<DetectedLocalPort>> = std::thread::scope(|s| {
        let handles: Vec<_> = COMMON_WIN_PROXY_CANDIDATES
            .iter()
            .map(|&(port, proxy_type, software)| {
                s.spawn(move || {
                    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
                    if TcpStream::connect_timeout(&addr, Duration::from_millis(220)).is_ok() {
                        Some(DetectedLocalPort {
                            endpoint: ProxyEndpoint {
                                proxy_type: proxy_type.to_string(),
                                host: "127.0.0.1".to_string(),
                                port,
                            },
                            software,
                        })
                    } else {
                        None
                    }
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    hits.into_iter().flatten().collect()
}

/// 使用系统 curl.exe 实际验证是否可通过指定代理（或直连）访问 Google
pub fn probe_google_access(proxy: Option<&ProxyEndpoint>) -> Result<(), String> {
    if let Some(ep) = proxy {
        if !check_proxy_reachable_timeout(ep, Duration::from_millis(700)) {
            return Err(format!("无法连接代理端口 {}:{}（未开启或被拒绝）", ep.host, ep.port));
        }
    }

    let mut last_err = String::new();
    for url in [GOOGLE_PROBE_URL_PRIMARY, GOOGLE_PROBE_URL_FALLBACK] {
        match run_curl_google_probe(url, proxy) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

fn run_curl_google_probe(url: &str, proxy: Option<&ProxyEndpoint>) -> Result<(), String> {
    let mut cmd = Command::new("curl.exe");
    cmd.args([
        "-sS",
        "--connect-timeout",
        "4",
        "--max-time",
        "6",
        "-o",
        "NUL",
        "-w",
        "%{http_code}",
        "-A",
        "AideRun/1.0",
    ]);

    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }

    if let Some(ep) = proxy {
        let lower_type = ep.proxy_type.to_ascii_lowercase();
        if lower_type.starts_with("socks") {
            cmd.arg("--socks5-hostname")
                .arg(format!("{}:{}", ep.host, ep.port));
        } else {
            cmd.arg("-x").arg(ep.http_url());
        }
    } else {
        cmd.arg("--noproxy").arg("*");
    }

    cmd.arg(url).creation_flags(CREATE_NO_WINDOW);

    let output = cmd
        .output()
        .map_err(|e| format!("调用 curl.exe 检测网络失败: {e}"))?;
    let code_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr_str = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if output.status.success() {
        let code: u16 = code_str.parse().unwrap_or(0);
        if code == 204 || code == 200 {
            return Ok(());
        }
        return Err(format!("访问 Google 返回异常 HTTP 状态码: {code_str}"));
    }

    if stderr_str.is_empty() {
        Err(format!("无法连接 Google（curl 退出码: {:?}）", output.status.code()))
    } else {
        Err(format!("无法连接 Google：{stderr_str}"))
    }
}

/// 启动时优先检测常见 Win 翻墙软件本地代理及系统配置，确认是否能正常访问 Google
pub fn auto_detect_google_proxy(paths: &PortablePaths, cfg: &LauncherConfig) -> GoogleProbeReport {
    let running_clients = detect_running_proxy_clients();
    let open_local_ports = scan_local_proxy_ports();

    let mut candidates: Vec<(ProxyEndpoint, String)> = Vec::new();
    let mut seen: HashSet<(String, String, u16)> = HashSet::new();

    let mut push_candidate = |ep: ProxyEndpoint, label: String| {
        let key = (
            ep.proxy_type.to_ascii_lowercase(),
            ep.host.to_ascii_lowercase(),
            ep.port,
        );
        if seen.insert(key) {
            candidates.push((ep, label));
        }
    };

    if let Some(manual) = cfg.proxy.manual_proxy.as_deref().and_then(parse_proxy_string) {
        push_candidate(manual, "已保存的自定义代理".to_string());
    }
    if let Some(cfg_ep) = read_config_json_proxy(&paths.proxy_config) {
        push_candidate(cfg_ep, "config.json 代理配置".to_string());
    }
    if let Some(sys_ep) = read_windows_system_proxy() {
        push_candidate(sys_ep, "Windows 系统代理".to_string());
    }
    if let Some(env_ep) = read_env_proxy() {
        push_candidate(env_ep, "系统环境变量代理".to_string());
    }
    for hit in &open_local_ports {
        push_candidate(
            hit.endpoint.clone(),
            format!("本机 {} ({})", hit.software, hit.endpoint.port),
        );
    }

    let mut failure_notes = Vec::<String>::new();

    for (ep, label) in &candidates {
        if !check_proxy_reachable_timeout(ep, Duration::from_millis(450)) {
            failure_notes.push(format!("{label} ({}:{}) 未监听", ep.host, ep.port));
            continue;
        }
        match probe_google_access(Some(ep)) {
            Ok(()) => {
                return GoogleProbeReport {
                    reachable: true,
                    endpoint: Some(ep.clone()),
                    source_label: label.clone(),
                    running_clients,
                    open_local_ports,
                    failure_reason: String::new(),
                };
            }
            Err(err) => {
                failure_notes.push(format!("{label} ({}): {err}", ep.all_proxy_url()));
            }
        }
    }

    // 兼容 TUN 模式 / 透明网关 / 软路由：无代理直连实测 Google
    match probe_google_access(None) {
        Ok(()) => GoogleProbeReport {
            reachable: true,
            endpoint: None,
            source_label: "直连 / TUN 透明代理模式".to_string(),
            running_clients,
            open_local_ports,
            failure_reason: String::new(),
        },
        Err(direct_err) => {
            if failure_notes.is_empty() {
                failure_notes.push(format!(
                    "未检测到开启的常见本地代理端口，且直连 Google 失败（{direct_err}）"
                ));
            } else {
                failure_notes.push(format!("直连检测失败（{direct_err}）"));
            }
            GoogleProbeReport {
                reachable: false,
                endpoint: None,
                source_label: String::new(),
                running_clients,
                open_local_ports,
                failure_reason: failure_notes.join("；"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_protocol_and_simple_proxy_strings() {
        let ep1 = parse_proxy_string("http=127.0.0.1:7890;https=127.0.0.1:7890;socks=127.0.0.1:10808")
            .unwrap();
        assert_eq!(ep1.proxy_type, "http");
        assert_eq!(ep1.host, "127.0.0.1");
        assert_eq!(ep1.port, 7890);

        let ep2 = parse_proxy_string("127.0.0.1:10808").unwrap();
        assert_eq!(ep2.proxy_type, "socks5");
        assert_eq!(ep2.port, 10808);

        let ep3 = parse_proxy_string("socks=127.0.0.1:10809").unwrap();
        assert_eq!(ep3.proxy_type, "socks5");
        assert_eq!(ep3.port, 10809);
    }

    #[test]
    fn resolves_locked_manual_proxy_over_other_sources() {
        let tmp = std::env::temp_dir().join(format!("aiderun_manual_proxy_{}.json", std::process::id()));
        let policy = ProxyPolicyConfig {
            sync_system_proxy: false,
            inject_env: true,
            lock_manual: true,
            manual_proxy: Some("http://127.0.0.1:7897".to_string()),
        };
        let ep = resolve_and_sync_proxy(&tmp, &policy).unwrap();
        assert_eq!(ep.proxy_type, "http");
        assert_eq!(ep.host, "127.0.0.1");
        assert_eq!(ep.port, 7897);
    }

    #[test]
    fn updates_config_json_preserving_unrelated_keys() {
        let tmp = std::env::temp_dir().join(format!("aiderun_proxy_{}.json", std::process::id()));
        std::fs::write(
            &tmp,
            "\u{FEFF}{\n  \"_comment\": \"Antigravity-Proxy\",\n  \"proxy\": { \"type\": \"socks5\", \"host\": \"127.0.0.1\", \"port\": 10808 },\n  \"fake_ip\": { \"enabled\": true }\n}",
        )
        .unwrap();

        let existing = read_config_json_proxy(&tmp).unwrap();
        assert_eq!(existing.port, 10808);

        let new_ep = ProxyEndpoint {
            proxy_type: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port: 7897,
        };
        update_config_json_proxy(&tmp, &new_ep).unwrap();

        let read_back: Value =
            serde_json::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(read_back["_comment"], "Antigravity-Proxy");
        assert_eq!(read_back["fake_ip"]["enabled"], true);
        assert_eq!(read_back["proxy"]["port"], 7897);
        assert_eq!(read_back["proxy"]["type"], "http");

        let _ = std::fs::remove_file(&tmp);
    }
}

