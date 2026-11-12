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

/// 不指定协议时可按端口直接优先推断为 SOCKS5 的常见端口
const KNOWN_SOCKS5_PORTS: &[u16] = &[1080, 10808, 2080, 7891, 2334, 20170, 6153, 1082, 9050, 9150];

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

/// 解析后的代理端点描述（含可选认证账号密码）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxyEndpoint {
    pub proxy_type: String,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl ProxyEndpoint {
    /// 构造无认证基础代理端点
    pub fn new(proxy_type: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self {
            proxy_type: proxy_type.into(),
            host: host.into(),
            port,
            username: None,
            password: None,
        }
    }

    /// 归一化协议名称
    pub fn normalized_scheme(&self) -> &'static str {
        let lower = self.proxy_type.trim().to_ascii_lowercase();
        if lower.starts_with("socks5") || lower == "socks" {
            "socks5"
        } else if lower.starts_with("socks4") {
            "socks4"
        } else if lower == "https" {
            "https"
        } else {
            "http"
        }
    }

    fn auth_url_prefix(&self) -> String {
        let Some(user) = self.username.as_deref().filter(|u| !u.is_empty()) else {
            return String::new();
        };
        let u_enc = encode_userinfo_component(user);
        match self.password.as_deref().filter(|p| !p.is_empty()) {
            Some(pass) => format!("{u_enc}:{}@", encode_userinfo_component(pass)),
            None => format!("{u_enc}@"),
        }
    }

    /// 生成 HTTP/HTTPS 代理 URL（若为 SOCKS 系列则透传其实际 SOCKS URL，防止对纯 SOCKS 端口误发 HTTP CONNECT）
    pub fn http_url(&self) -> String {
        let scheme = self.normalized_scheme();
        if scheme.starts_with("socks") {
            self.all_proxy_url()
        } else {
            format!("{scheme}://{}{}:{}", self.auth_url_prefix(), self.host, self.port)
        }
    }

    /// 生成完整代理 URL（含可选用户名密码）
    pub fn all_proxy_url(&self) -> String {
        format!(
            "{}://{}{}:{}",
            self.normalized_scheme(),
            self.auth_url_prefix(),
            self.host,
            self.port
        )
    }

    /// 生成用于 UI 与日志展示的脱敏代理地址（隐藏明文密码）
    pub fn masked_url(&self) -> String {
        let scheme = self.normalized_scheme();
        if let Some(user) = self.username.as_deref().filter(|u| !u.is_empty()) {
            if self.password.as_deref().is_some_and(|p| !p.is_empty()) {
                return format!("{scheme}://{user}:***@{}:{}", self.host, self.port);
            }
            return format!("{scheme}://{user}@{}:{}", self.host, self.port);
        }
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    /// 返回切换为指定协议后的副本
    pub fn with_scheme(&self, scheme: &str) -> Self {
        let mut cloned = self.clone();
        cloned.proxy_type = scheme.to_string();
        cloned
    }
}

/// 带“协议是否显式指定”标记的解析结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedProxyInput {
    pub endpoint: ProxyEndpoint,
    pub scheme_explicit: bool,
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

/// 智能解析客户粘贴的多格式单行/多行代理字符串（自动提取协议、服务器、端口、用户名与密码）
pub fn parse_proxy_string(raw: &str) -> Option<ProxyEndpoint> {
    parse_proxy_input(raw).map(|p| p.endpoint)
}

/// 生成待探测的候选代理序列：若输入未显式写明协议，将按常见端口推断主协议并在连接握手失败时回退另一协议
pub fn parse_proxy_candidates(raw: &str) -> Vec<ProxyEndpoint> {
    let Some(parsed) = parse_proxy_input(raw) else {
        return Vec::new();
    };
    if parsed.scheme_explicit {
        return vec![parsed.endpoint];
    }
    let alt_scheme = if parsed.endpoint.normalized_scheme().starts_with("socks") {
        "http"
    } else {
        "socks5"
    };
    let fallback = parsed.endpoint.with_scheme(alt_scheme);
    vec![parsed.endpoint, fallback]
}

/// 智能识别多种单行/复制文本代理格式并记录协议是否由用户输入显式指定
pub fn parse_proxy_input(raw: &str) -> Option<ParsedProxyInput> {
    let cleaned = strip_shell_prefix_and_quotes(raw);
    if cleaned.is_empty() {
        return None;
    }

    // 1. 先检查键值对文本块（如“服务器：... 端口：... 用户名：... 密码：...”或“host=... port=...”）
    if let Some(kv) = try_parse_kv_proxy_snippet(&cleaned) {
        return Some(kv);
    }

    // 2. 兼容 Windows 系统代理多协议分号格式：`http=127.0.0.1:7890;socks=127.0.0.1:10808`
    if cleaned.contains('=') && !cleaned.contains("://") {
        if let Some(ep) = try_parse_windows_registry_proxy(&cleaned) {
            return Some(ParsedProxyInput {
                endpoint: ep,
                scheme_explicit: true,
            });
        }
    }

    // 3. 多令牌分隔行：`socks5 1.2.3.4 10808 user pass` / `1.2.3.4|8080|user|pass` / `1.2.3.4 8080`
    if cleaned.contains(|c: char| c.is_whitespace() || c == '|' || c == ',') {
        if let Some(res) = try_parse_delimited_tokens(&cleaned) {
            return Some(res);
        }
    }

    // 4. 标准 URI、带 @ 认证或 IP:端口(:用户名:密码) 单串
    parse_single_token_proxy(&cleaned, None)
}

fn strip_shell_prefix_and_quotes(raw: &str) -> String {
    let mut s = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | ',' | ';'))
        .trim();

    for prefix in ["export ", "EXPORT ", "set ", "SET "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
        }
    }
    for env_key in [
        "HTTPS_PROXY=",
        "https_proxy=",
        "HTTP_PROXY=",
        "http_proxy=",
        "ALL_PROXY=",
        "all_proxy=",
    ] {
        if let Some(rest) = s.strip_prefix(env_key) {
            s = rest.trim();
        }
    }
    s.trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .trim()
        .to_string()
}

fn normalize_scheme_keyword(raw: &str) -> Option<&'static str> {
    let clean = raw.trim().to_ascii_lowercase();
    if clean.starts_with("socks5") || clean == "socks" || clean == "sock5" {
        Some("socks5")
    } else if clean.starts_with("socks4") {
        Some("socks4")
    } else if clean == "https" {
        Some("https")
    } else if clean == "http" {
        Some("http")
    } else {
        None
    }
}

fn default_scheme_for_port(port: u16) -> &'static str {
    if KNOWN_SOCKS5_PORTS.contains(&port) {
        "socks5"
    } else {
        "http"
    }
}

fn looks_like_host(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.len() > 253 {
        return false;
    }
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return trimmed.len() > 2;
    }
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    trimmed.contains('.')
        && !trimmed.starts_with('.')
        && !trimmed.ends_with('.')
        && !trimmed.contains(|c: char| c.is_whitespace() || c == '@' || c == '/')
}

fn parse_port_str(s: &str) -> Option<u16> {
    let clean = s.trim().trim_end_matches('/');
    if clean.is_empty() || !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let port = clean.parse::<u16>().ok()?;
    (port > 0).then_some(port)
}

fn build_endpoint_result(
    scheme_hint: Option<&str>,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
) -> Option<ParsedProxyInput> {
    let host = host.trim().to_string();
    if host.is_empty() || port == 0 || host.contains(char::is_whitespace) {
        return None;
    }
    let scheme_explicit = scheme_hint.is_some();
    let proxy_type = scheme_hint
        .map(str::to_string)
        .unwrap_or_else(|| default_scheme_for_port(port).to_string());

    let username = username
        .map(|u| decode_percent_component(u.trim()))
        .filter(|u| !u.is_empty());
    let password = password
        .map(|p| decode_percent_component(p.trim()))
        .filter(|p| !p.is_empty());

    Some(ParsedProxyInput {
        endpoint: ProxyEndpoint {
            proxy_type,
            host,
            port,
            username,
            password,
        },
        scheme_explicit,
    })
}

fn try_parse_windows_registry_proxy(raw: &str) -> Option<ProxyEndpoint> {
    let mut fallback: Option<ProxyEndpoint> = None;
    for part in raw.split(';') {
        let item = part.trim();
        let Some((proto_raw, addr_raw)) = item.split_once('=') else {
            continue;
        };
        let Some(scheme) = normalize_scheme_keyword(proto_raw) else {
            continue;
        };
        if let Some((host, port)) = split_host_and_port(addr_raw.trim()) {
            let ep = ProxyEndpoint::new(scheme, host, port);
            if scheme == "http" {
                return Some(ep);
            }
            if fallback.is_none() {
                fallback = Some(ep);
            }
        }
    }
    fallback
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KvFieldKind {
    Scheme,
    Host,
    Port,
    User,
    Pass,
}

fn try_parse_kv_proxy_snippet(text: &str) -> Option<ParsedProxyInput> {
    const KEY_DEFS: &[(&str, KvFieldKind)] = &[
        ("服务器地址", KvFieldKind::Host),
        ("代理服务器", KvFieldKind::Host),
        ("主机地址", KvFieldKind::Host),
        ("服务器", KvFieldKind::Host),
        ("主机", KvFieldKind::Host),
        ("地址", KvFieldKind::Host),
        ("ip地址", KvFieldKind::Host),
        ("server", KvFieldKind::Host),
        ("host", KvFieldKind::Host),
        ("ip", KvFieldKind::Host),
        ("端口号", KvFieldKind::Port),
        ("端口", KvFieldKind::Port),
        ("port", KvFieldKind::Port),
        ("协议类型", KvFieldKind::Scheme),
        ("代理协议", KvFieldKind::Scheme),
        ("协议", KvFieldKind::Scheme),
        ("类型", KvFieldKind::Scheme),
        ("protocol", KvFieldKind::Scheme),
        ("scheme", KvFieldKind::Scheme),
        ("proto", KvFieldKind::Scheme),
        ("type", KvFieldKind::Scheme),
        ("用户名", KvFieldKind::User),
        ("账号", KvFieldKind::User),
        ("用户", KvFieldKind::User),
        ("username", KvFieldKind::User),
        ("account", KvFieldKind::User),
        ("user", KvFieldKind::User),
        ("密码", KvFieldKind::Pass),
        ("口令", KvFieldKind::Pass),
        ("password", KvFieldKind::Pass),
        ("passwd", KvFieldKind::Pass),
        ("pass", KvFieldKind::Pass),
        ("pwd", KvFieldKind::Pass),
    ];

    let mut matches: Vec<(usize, usize, KvFieldKind)> = Vec::new();
    let lower = text.to_ascii_lowercase();

    for (idx, _) in text.char_indices() {
        let prev_char = text[..idx].chars().next_back();
        if let Some(prev) = prev_char {
            let boundary_ok = !prev.is_ascii_alphanumeric();
            if !boundary_ok {
                continue;
            }
        }
        let rem_text = &text[idx..];
        let rem_lower = &lower[idx..];
        for &(kw, kind) in KEY_DEFS {
            let matched = if kw.is_ascii() {
                rem_lower.starts_with(kw)
            } else {
                rem_text.starts_with(kw)
            };
            if !matched {
                continue;
            }
            let after = &rem_text[kw.len()..];
            let trimmed_after = after.trim_start_matches(|c: char| c == ' ' || c == '\t');
            if let Some(sep_len) = trimmed_after
                .chars()
                .next()
                .filter(|&c| matches!(c, ':' | '：' | '='))
                .map(char::len_utf8)
            {
                let ws_len = after.len() - trimmed_after.len();
                matches.push((idx, idx + kw.len() + ws_len + sep_len, kind));
                break;
            }
        }
    }

    if matches.len() < 2 {
        return None;
    }

    matches.sort_by_key(|m| m.0);
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut scheme: Option<&'static str> = None;
    let mut username: Option<String> = None;
    let mut password: Option<String> = None;

    for i in 0..matches.len() {
        let (_, val_start, kind) = matches[i];
        let val_end = if i + 1 < matches.len() {
            matches[i + 1].0
        } else {
            text.len()
        };
        if val_start >= val_end {
            continue;
        }
        let val = text[val_start..val_end]
            .trim()
            .trim_matches(|c: char| matches!(c, ',' | ';' | '，' | '；' | '|' | '"' | '\''))
            .trim();
        if val.is_empty() {
            continue;
        }
        match kind {
            KvFieldKind::Host if host.is_none() => {
                if let Some((h, p)) = split_host_and_port(val) {
                    host = Some(h);
                    if port.is_none() {
                        port = Some(p);
                    }
                } else {
                    host = Some(val.to_string());
                }
            }
            KvFieldKind::Port if port.is_none() => port = parse_port_str(val),
            KvFieldKind::Scheme if scheme.is_none() => scheme = normalize_scheme_keyword(val),
            KvFieldKind::User if username.is_none() => username = Some(val.to_string()),
            KvFieldKind::Pass if password.is_none() => password = Some(val.to_string()),
            _ => {}
        }
    }

    build_endpoint_result(scheme, host?, port?, username, password)
}

fn try_parse_delimited_tokens(cleaned: &str) -> Option<ParsedProxyInput> {
    let mut tokens: Vec<&str> = cleaned
        .split(|c: char| c.is_whitespace() || c == '|' || c == ',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }

    let mut explicit_scheme: Option<&'static str> = None;
    if let Some(first) = tokens.first().and_then(|t| normalize_scheme_keyword(t)) {
        explicit_scheme = Some(first);
        tokens.remove(0);
    } else if let Some(last) = tokens.last().and_then(|t| normalize_scheme_keyword(t)) {
        explicit_scheme = Some(last);
        tokens.pop();
    }

    match tokens.as_slice() {
        [single] => parse_single_token_proxy(single, explicit_scheme),
        [a, b] => {
            if let Some(p) = parse_port_str(b) {
                build_endpoint_result(explicit_scheme, (*a).to_string(), p, None, None)
            } else {
                parse_single_token_proxy(a, explicit_scheme)
            }
        }
        [a, b, c] => {
            if let Some(parsed) = parse_single_token_proxy(a, explicit_scheme) {
                let mut ep = parsed.endpoint;
                if ep.username.is_none() {
                    ep.username = Some((*b).to_string());
                    ep.password = Some((*c).to_string());
                }
                Some(ParsedProxyInput {
                    endpoint: ep,
                    scheme_explicit: parsed.scheme_explicit,
                })
            } else if let Some(p) = parse_port_str(b) {
                build_endpoint_result(
                    explicit_scheme,
                    (*a).to_string(),
                    p,
                    Some((*c).to_string()),
                    None,
                )
            } else {
                None
            }
        }
        [a, b, c, d] => {
            if let Some(p) = parse_port_str(b) {
                build_endpoint_result(
                    explicit_scheme,
                    (*a).to_string(),
                    p,
                    Some((*c).to_string()),
                    Some((*d).to_string()),
                )
            } else if let Some(p) = parse_port_str(d) {
                if looks_like_host(c) {
                    build_endpoint_result(
                        explicit_scheme,
                        (*c).to_string(),
                        p,
                        Some((*a).to_string()),
                        Some((*b).to_string()),
                    )
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_single_token_proxy(
    raw: &str,
    fallback_scheme: Option<&'static str>,
) -> Option<ParsedProxyInput> {
    let trimmed = raw.trim();
    let (scheme_from_uri, rest) = if let Some((scheme_raw, body)) = trimmed.split_once("://") {
        (Some(normalize_scheme_keyword(scheme_raw)?), body.trim())
    } else {
        (None, trimmed)
    };
    let effective_scheme = scheme_from_uri.or(fallback_scheme);

    // 存在 `@`：右侧为服务器[:端口]，左侧为用户名[:密码]（使用 rsplit_once 容忍密码中含 `@`）
    if let Some((auth_part, addr_part)) = rest.rsplit_once('@') {
        let addr_clean = strip_url_path_suffix(addr_part);
        let (host, port) = split_host_and_port(addr_clean)?;
        let (user_raw, pass_raw) = match auth_part.split_once(':') {
            Some((u, p)) => (u, Some(p.to_string())),
            None => (auth_part, None),
        };
        return build_endpoint_result(
            effective_scheme,
            host,
            port,
            Some(user_raw.to_string()),
            pass_raw,
        );
    }

    let clean_rest = strip_url_path_suffix(rest);
    if clean_rest.starts_with('[') {
        let (host, port) = split_host_and_port(clean_rest)?;
        return build_endpoint_result(effective_scheme, host, port, None, None);
    }

    let colon_count = clean_rest.bytes().filter(|&b| b == b':').count();
    if colon_count == 1 {
        let (host, port) = split_host_and_port(clean_rest)?;
        return build_endpoint_result(effective_scheme, host, port, None, None);
    }

    // 兼容无 `@` 多冒号出口格式：`ip:port:user:pass` / `ip:port:user` / `user:pass:ip:port`
    if colon_count >= 2 {
        let parts: Vec<&str> = clean_rest.splitn(4, ':').collect();
        if parts.len() >= 3 {
            if let Some(p1) = parse_port_str(parts[1]) {
                if looks_like_host(parts[0]) {
                    return build_endpoint_result(
                        effective_scheme,
                        parts[0].to_string(),
                        p1,
                        Some(parts[2].to_string()),
                        parts.get(3).map(|s| (*s).to_string()),
                    );
                }
            }
        }
        if parts.len() == 4 {
            if let Some(p3) = parse_port_str(parts[3]) {
                if looks_like_host(parts[2]) {
                    return build_endpoint_result(
                        effective_scheme,
                        parts[2].to_string(),
                        p3,
                        Some(parts[0].to_string()),
                        Some(parts[1].to_string()),
                    );
                }
            }
        }
    }

    None
}

fn strip_url_path_suffix(addr: &str) -> &str {
    let cut = addr.find(['/', '?', '#']).unwrap_or(addr.len());
    addr[..cut].trim()
}

fn split_host_and_port(input: &str) -> Option<(String, u16)> {
    let clean = strip_url_path_suffix(input.trim());
    if clean.starts_with('[') {
        let end = clean.find("]:")?;
        let inner = clean[1..end].trim();
        if inner.is_empty() {
            return None;
        }
        let port = parse_port_str(&clean[end + 2..])?;
        return Some((format!("[{inner}]"), port));
    }
    let (host, port_str) = clean.rsplit_once(':')?;
    let host = host.trim();
    if host.is_empty() || host.contains(':') {
        return None;
    }
    let port = parse_port_str(port_str)?;
    Some((host.to_string(), port))
}

fn encode_userinfo_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'!');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode_percent_component(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
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
    let username = proxy
        .get("username")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let password = proxy
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if host.is_empty() || port == 0 {
        return None;
    }
    Some(ProxyEndpoint {
        proxy_type,
        host,
        port,
        username,
        password,
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
        let mut proxy_obj = serde_json::Map::new();
        proxy_obj.insert("type".to_string(), serde_json::json!(ep.normalized_scheme()));
        proxy_obj.insert("host".to_string(), serde_json::json!(ep.host));
        proxy_obj.insert("port".to_string(), serde_json::json!(ep.port));
        if let Some(user) = ep.username.as_deref().filter(|u| !u.is_empty()) {
            proxy_obj.insert("username".to_string(), serde_json::json!(user));
        }
        if let Some(pass) = ep.password.as_deref().filter(|p| !p.is_empty()) {
            proxy_obj.insert("password".to_string(), serde_json::json!(pass));
        }
        obj.insert("proxy".to_string(), Value::Object(proxy_obj));
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
                            endpoint: ProxyEndpoint::new(proxy_type, "127.0.0.1", port),
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
        let scheme = ep.normalized_scheme();
        if scheme.starts_with("socks") {
            let flag = if scheme == "socks4" {
                "--socks4"
            } else {
                "--socks5-hostname"
            };
            cmd.arg(flag).arg(format!("{}:{}", ep.host, ep.port));
        } else {
            cmd.arg("-x").arg(format!("{scheme}://{}:{}", ep.host, ep.port));
        }
        if let Some(user) = ep.username.as_deref().filter(|u| !u.is_empty()) {
            let cred = match ep.password.as_deref() {
                Some(pass) => format!("{user}:{pass}"),
                None => user.to_string(),
            };
            cmd.arg("--proxy-user").arg(cred);
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

        let new_ep = ProxyEndpoint::new("http", "127.0.0.1", 7897);
        update_config_json_proxy(&tmp, &new_ep).unwrap();

        let read_back: Value =
            serde_json::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(read_back["_comment"], "Antigravity-Proxy");
        assert_eq!(read_back["fake_ip"]["enabled"], true);
        assert_eq!(read_back["proxy"]["port"], 7897);
        assert_eq!(read_back["proxy"]["type"], "http");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn intelligently_parses_authenticated_and_multi_format_proxy_strings() {
        let url_auth = parse_proxy_input("socks5://alice:p%40ss@203.0.113.10:1080/").unwrap();
        assert!(url_auth.scheme_explicit);
        assert_eq!(url_auth.endpoint.normalized_scheme(), "socks5");
        assert_eq!(url_auth.endpoint.host, "203.0.113.10");
        assert_eq!(url_auth.endpoint.port, 1080);
        assert_eq!(url_auth.endpoint.username.as_deref(), Some("alice"));
        assert_eq!(url_auth.endpoint.password.as_deref(), Some("p@ss"));
        assert_eq!(url_auth.endpoint.all_proxy_url(), "socks5://alice:p%40ss@203.0.113.10:1080");
        assert_eq!(url_auth.endpoint.http_url(), "socks5://alice:p%40ss@203.0.113.10:1080");
        assert_eq!(url_auth.endpoint.masked_url(), "socks5://alice:***@203.0.113.10:1080");

        let colon_4 = parse_proxy_input("45.32.10.2:8080:bob:sec123").unwrap();
        assert!(!colon_4.scheme_explicit);
        assert_eq!(colon_4.endpoint.host, "45.32.10.2");
        assert_eq!(colon_4.endpoint.port, 8080);
        assert_eq!(colon_4.endpoint.username.as_deref(), Some("bob"));
        assert_eq!(colon_4.endpoint.password.as_deref(), Some("sec123"));

        let cands = parse_proxy_candidates("45.32.10.2:33445:bob:sec123");
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].normalized_scheme(), "http");
        assert_eq!(cands[1].normalized_scheme(), "socks5");

        let kv = parse_proxy_input("协议：SOCKS5 服务器：103.5.6.7 端口：10808 用户名：u01 密码：p@01").unwrap();
        assert!(kv.scheme_explicit);
        assert_eq!(kv.endpoint.normalized_scheme(), "socks5");
        assert_eq!(kv.endpoint.host, "103.5.6.7");
        assert_eq!(kv.endpoint.port, 10808);
        assert_eq!(kv.endpoint.username.as_deref(), Some("u01"));
        assert_eq!(kv.endpoint.password.as_deref(), Some("p@01"));

        let piped = parse_proxy_string("http | 10.0.0.5 | 8888 | usr | pwd").unwrap();
        assert_eq!(piped.normalized_scheme(), "http");
        assert_eq!(piped.host, "10.0.0.5");
        assert_eq!(piped.port, 8888);
        assert_eq!(piped.username.as_deref(), Some("usr"));
        assert_eq!(piped.password.as_deref(), Some("pwd"));
    }
}

