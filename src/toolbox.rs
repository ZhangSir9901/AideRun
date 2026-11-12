// AideRun - 零软链接原生便携组件综合管理引擎（安装状态/版本检测/升降级/卸载/自定义）

use std::cmp::Ordering;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use walkdir::WalkDir;

use crate::config::{CustomComponent, LauncherConfig};
use crate::console::CREATE_NO_WINDOW;
use crate::env::{apply_env_to_command, build_isolated_env};
use crate::net::{download_file, extract_archive_clean, http_get_text};
use crate::paths::PortablePaths;
use crate::proxy::ProxyEndpoint;
use crate::scoop_heal::auto_heal_scoop_if_needed;
use crate::upgrade::compare_semver;

/// 内置原生零软链接便携组件定义
#[derive(Debug, Clone)]
pub struct BuiltinToolSpec {
    pub name: &'static str,
    pub display_name: &'static str,
    pub summary: &'static str,
    pub fallback_url: &'static str,
    pub show_in_panel: bool,
}

pub const BUILTIN_PORTABLE_TOOLS: &[BuiltinToolSpec] = &[
    BuiltinToolSpec {
        name: "innounp",
        display_name: "innounp-2",
        summary: "Inno Setup 6.4+ 零注册表安装包解包引擎",
        fallback_url: "https://raw.githubusercontent.com/jrathlev/InnoUnpacker-Windows-GUI/refs/heads/master/innounp-2/bin/innounp-2.zip",
        show_in_panel: true,
    },
    BuiltinToolSpec {
        name: "uv",
        display_name: "uv",
        summary: "超快单文件 Python 包管理与虚拟环境工具",
        fallback_url: "https://github.com/astral-sh/uv/releases/latest/download/uv-x86_64-pc-windows-msvc.zip",
        show_in_panel: true,
    },
    BuiltinToolSpec {
        name: "7zip",
        display_name: "7-Zip (7zr)",
        summary: "独立控制台压缩与解包工具（零软链接）",
        fallback_url: "https://github.com/ip7z/7zip/releases/download/26.06/7zr.exe",
        show_in_panel: true,
    },
    BuiltinToolSpec {
        name: "mingit",
        display_name: "MinGit",
        summary: "Git for Windows 精简纯绿色便携版",
        fallback_url: "https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.1/MinGit-2.56.0-64-bit.zip",
        show_in_panel: true,
    },
    BuiltinToolSpec {
        name: "nodejs",
        display_name: "Node.js LTS",
        summary: "Node.js 官方长期支持 Win-x64 绿色版",
        fallback_url: "",
        show_in_panel: true,
    },
    BuiltinToolSpec {
        name: "innoextract",
        display_name: "innoextract",
        summary: "Inno Setup 经典兼容备选解包工具",
        fallback_url: "https://constexpr.org/innoextract/files/innoextract-1.9/innoextract-1.9-windows.zip",
        show_in_panel: false,
    },
];

/// 控制面板「组件管理」综合视图单项状态
#[derive(Debug, Clone, Serialize)]
pub struct ComponentStatusItem {
    pub id: String,
    pub display_name: String,
    pub summary: String,
    pub builtin: bool,
    pub installed: bool,
    pub local_version: Option<String>,
    pub remote_version: Option<String>,
    /// 状态标识: "not_installed" | "up_to_date" | "updatable" | "installed_unknown" | "custom"
    pub update_state: &'static str,
    pub install_dir: String,
    pub custom_url: Option<String>,
}

/// 采集内置组件与自定义组件的安装状态、本地版本及（可选）远程最新版本对比
pub fn collect_component_statuses(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    fetch_remote: bool,
    cached_remote: Option<&HashMap<String, String>>,
) -> Vec<ComponentStatusItem> {
    let mut remote_versions: HashMap<String, String> = cached_remote.cloned().unwrap_or_default();

    if fetch_remote {
        let builtin_ids: Vec<&str> = BUILTIN_PORTABLE_TOOLS
            .iter()
            .filter(|t| t.show_in_panel)
            .map(|t| t.name)
            .collect();

        let fetched: Vec<(&str, Option<String>)> = std::thread::scope(|s| {
            let handles: Vec<_> = builtin_ids
                .iter()
                .map(|&id| {
                    s.spawn(move || {
                        let ver = resolve_remote_info(id).map(|(v, _)| v);
                        (id, ver)
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });

        for (id, ver_opt) in fetched {
            if let Some(v) = ver_opt {
                remote_versions.insert(id.to_string(), v);
            }
        }
    }

    let mut list = Vec::new();

    for spec in BUILTIN_PORTABLE_TOOLS.iter().filter(|t| t.show_in_panel) {
        let dir = paths.portable_tools_dir.join(spec.name);
        let (installed, local_ver, install_dir) = probe_builtin_installed(paths, spec.name, &dir);
        let remote_ver = remote_versions.get(spec.name).cloned();
        let update_state = compute_update_state(installed, local_ver.as_deref(), remote_ver.as_deref(), true);

        list.push(ComponentStatusItem {
            id: spec.name.to_string(),
            display_name: spec.display_name.to_string(),
            summary: spec.summary.to_string(),
            builtin: true,
            installed,
            local_version: local_ver,
            remote_version: remote_ver,
            update_state,
            install_dir,
            custom_url: None,
        });
    }

    for custom in &cfg.toolchain.custom_components {
        let key = custom.name.trim().to_ascii_lowercase();
        let dir = paths.portable_tools_dir.join(&key);
        let installed = dir.is_dir() && dir_has_files(&dir);
        let local_ver = if installed {
            read_version_receipt(&dir).or_else(|| Some("已安装".to_string()))
        } else {
            None
        };
        let update_state = if installed { "custom" } else { "not_installed" };

        list.push(ComponentStatusItem {
            id: custom.name.clone(),
            display_name: custom.name.clone(),
            summary: if custom.description.trim().is_empty() {
                "自定义便携绿色组件".to_string()
            } else {
                custom.description.clone()
            },
            builtin: false,
            installed,
            local_version: local_ver,
            remote_version: Some("自定义源".to_string()),
            update_state,
            install_dir: paths.rel_display(&dir),
            custom_url: Some(custom.url.clone()),
        });
    }

    list
}

fn compute_update_state(
    installed: bool,
    local: Option<&str>,
    remote: Option<&str>,
    builtin: bool,
) -> &'static str {
    if !installed {
        return "not_installed";
    }
    if !builtin {
        return "custom";
    }
    match (local, remote) {
        (Some(loc), Some(rem)) => match compare_semver(rem, loc) {
            Ordering::Greater => "updatable",
            _ => "up_to_date",
        },
        _ => "installed_unknown",
    }
}

/// 处理 `tools` 子命令
pub fn handle_tools_command(
    args: &[String],
    paths: &PortablePaths,
    cfg: &mut LauncherConfig,
    proxy: Option<&ProxyEndpoint>,
) -> Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "list" | "ls" => list_tools(paths, cfg, proxy),
        "install" | "add" | "upgrade" => {
            let name = args
                .get(1)
                .context("用法: AideRun.exe tools install <组件名> [--url <下载地址>]")?;
            let custom_url = parse_flag_value(args, "--url");
            install_portable_tool(paths, cfg, name, custom_url.as_deref())
        }
        "remove" | "rm" | "uninstall" => {
            let name = args
                .get(1)
                .context("用法: AideRun.exe tools remove <组件名>")?;
            remove_portable_tool(paths, cfg, name)
        }
        "add-custom" => {
            let name = args
                .get(1)
                .context("用法: AideRun.exe tools add-custom <组件名> --url <URL> [--bin <子路径>]")?;
            let url = parse_flag_value(args, "--url")
                .context("请提供 --url <压缩包或单文件直链>")?;
            let bin_sub = parse_flag_value(args, "--bin");
            add_custom_component(paths, cfg, name, &url, bin_sub, None)?;
            install_portable_tool(paths, cfg, name, Some(&url))
        }
        "heal" => {
            let report = auto_heal_scoop_if_needed(paths, true)?;
            println!(
                "[√] 跨盘符路径兼容检查完成（NTFS: {}，修复软链接: {}，重写 Shim: {}）",
                if report.is_ntfs { "是" } else { "否" },
                report.junctions_repaired,
                report.shims_rewritten
            );
            Ok(())
        }
        "scoop" => run_scoop_passthrough(&args[1..], paths, cfg, proxy),
        _ => {
            print_tools_help();
            Ok(())
        }
    }
}

/// 列出当前已部署的原生零软链接组件及版本探测结果
pub fn list_tools(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    _proxy: Option<&ProxyEndpoint>,
) -> Result<()> {
    println!("=== 1. 原生零软链接便携组件状态 (tools/portable) ===");
    let statuses = collect_component_statuses(paths, cfg, false, None);
    for item in statuses {
        let state = if item.installed { "已安装" } else { "未安装" };
        let ver = item.local_version.as_deref().unwrap_or("--");
        println!(
            "  [{state}] {:<14} 当前版本: {:<12} 目录: {}",
            item.display_name, ver, item.install_dir
        );
    }

    println!("\n=== 2. 可通过命令行一键安装/升级的组件 ID ===");
    for spec in BUILTIN_PORTABLE_TOOLS {
        println!("  - {:<12} : {}", spec.name, spec.summary);
    }
    Ok(())
}

/// 安装或升级内置或自定义原生便携组件至 tools/portable/<name>/（0 软链接）
pub fn install_portable_tool(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    name: &str,
    override_url: Option<&str>,
) -> Result<()> {
    let key = normalize_component_key(name);
    let (url, resolved_ver) = if let Some(u) = override_url {
        (u.to_string(), None)
    } else if let Some(custom) = cfg
        .toolchain
        .custom_components
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(&key) || c.name.eq_ignore_ascii_case(name.trim()))
    {
        (custom.url.clone(), None)
    } else {
        resolve_builtin_remote_or_fallback(&key)?
    };

    let target_dir = paths.portable_tools_dir.join(&key);
    std::fs::create_dir_all(&target_dir)?;

    let file_name = url
        .split('/')
        .last()
        .and_then(|s| s.split('?').next())
        .filter(|s| !s.is_empty())
        .unwrap_or("package.zip");
    let temp_file = paths
        .temp_dir
        .join(format!("tool_{}_{}", key, file_name));
    std::fs::create_dir_all(&paths.temp_dir)?;

    println!("[*] 正在下载组件 {key} -> {url}");
    download_file(&url, &temp_file, true)?;

    if file_name.to_ascii_lowercase().ends_with(".exe") {
        // 单文件工具（如 7zr.exe）先清空旧文件再移入
        let _ = std::fs::remove_dir_all(&target_dir);
        std::fs::create_dir_all(&target_dir)?;
        let final_exe = target_dir.join(file_name);
        std::fs::rename(&temp_file, &final_exe)?;
    } else {
        println!("[*] 正在解压至 {} ...", paths.rel_display(&target_dir));
        let _ = std::fs::remove_dir_all(&target_dir);
        std::fs::create_dir_all(&target_dir)?;
        extract_archive_clean(&temp_file, &target_dir, paths, true)?;
        let _ = std::fs::remove_file(&temp_file);
    }

    if let Some(ver) = resolved_ver {
        let _ = std::fs::write(target_dir.join(".aide_version"), format!("{ver}\n"));
    }

    println!("[√] 原生便携组件 {key} 部署完成！（零软链接、跨盘符通用）");
    Ok(())
}

/// 从 tools/portable/ 卸载组件
pub fn remove_portable_tool(
    paths: &PortablePaths,
    cfg: &mut LauncherConfig,
    name: &str,
) -> Result<()> {
    let key = normalize_component_key(name);
    let target = paths.portable_tools_dir.join(&key);
    if target.is_dir() {
        std::fs::remove_dir_all(&target)?;
        println!("[√] 已卸载组件目录: {}", paths.rel_display(&target));
    } else {
        let fallback = paths.portable_tools_dir.join(name.trim());
        if fallback.is_dir() {
            std::fs::remove_dir_all(&fallback)?;
            println!("[√] 已卸载组件目录: {}", paths.rel_display(&fallback));
        } else {
            println!("[-] 未找到已安装的原生组件目录: {name}");
        }
    }
    let before = cfg.toolchain.custom_components.len();
    cfg.toolchain
        .custom_components
        .retain(|c| !c.name.eq_ignore_ascii_case(name.trim()) && !c.name.eq_ignore_ascii_case(&key));
    if cfg.toolchain.custom_components.len() != before {
        cfg.save(&paths.launcher_config)?;
    }
    Ok(())
}

/// 保存并校验自定义便携组件定义到 data/launcher.toml
pub fn add_custom_component(
    paths: &PortablePaths,
    cfg: &mut LauncherConfig,
    name: &str,
    url: &str,
    bin_sub: Option<String>,
    desc: Option<String>,
) -> Result<()> {
    let clean_name: String = name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(32)
        .collect();
    if clean_name.is_empty() {
        anyhow::bail!("组件名称不能为空且仅支持字母、数字、点、- 或 _");
    }
    let clean_url = url.trim();
    if !clean_url.starts_with("https://") && !clean_url.starts_with("http://") {
        anyhow::bail!("下载地址必须以 https:// 或 http:// 开头");
    }

    cfg.toolchain
        .custom_components
        .retain(|c| !c.name.eq_ignore_ascii_case(&clean_name));
    cfg.toolchain.custom_components.push(CustomComponent {
        name: clean_name.clone(),
        url: clean_url.to_string(),
        bin_subpaths: bin_sub.into_iter().collect(),
        description: desc
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "用户自定义便携组件".to_string()),
    });
    cfg.save(&paths.launcher_config)?;
    println!("[√] 已保存自定义组件 {clean_name} 到 data/launcher.toml");
    Ok(())
}

fn normalize_component_key(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "node" | "nodejs" => "nodejs".to_string(),
        "git" | "mingit" => "mingit".to_string(),
        "7z" | "7zip" => "7zip".to_string(),
        "innounp-2" | "innounp" => "innounp".to_string(),
        other => other.to_string(),
    }
}

fn probe_builtin_installed(
    paths: &PortablePaths,
    id: &str,
    managed_dir: &Path,
) -> (bool, Option<String>, String) {
    // 优先检查受管理的 tools/portable/<id>
    if managed_dir.is_dir() && dir_has_files(managed_dir) {
        let ver = probe_managed_version(id, managed_dir).or_else(|| read_version_receipt(managed_dir));
        return (true, ver, paths.rel_display(managed_dir));
    }

    // 针对 innounp / 7zip 兼容识别直接在 tools/ 根目录放置的单文件情况
    if id == "innounp" {
        let legacy = paths.tools_dir.join("innounp.exe");
        if legacy.is_file() {
            let ver = run_version_cmd(&legacy, &[]).and_then(|out| parse_innounp_version(&out));
            return (true, ver, paths.rel_display(&legacy));
        }
    } else if id == "7zip" {
        for exe in [paths.tools_dir.join("7zr.exe"), paths.tools_dir.join("7z.exe")] {
            if exe.is_file() {
                let ver = run_version_cmd(&exe, &[]).and_then(|out| parse_7zip_output_version(&out));
                return (true, ver, paths.rel_display(&exe));
            }
        }
    }

    (false, None, paths.rel_display(managed_dir))
}

fn probe_managed_version(id: &str, dir: &Path) -> Option<String> {
    match id {
        "innounp" => {
            let exe = find_exe_recursive(dir, "innounp.exe")?;
            let out = run_version_cmd(&exe, &[])?;
            parse_innounp_version(&out)
        }
        "uv" => {
            let exe = find_exe_recursive(dir, "uv.exe")?;
            let out = run_version_cmd(&exe, &["--version"])?;
            extract_first_semver(&out)
        }
        "7zip" => {
            let exe = find_exe_recursive(dir, "7zr.exe")
                .or_else(|| find_exe_recursive(dir, "7z.exe"))?;
            let out = run_version_cmd(&exe, &[])?;
            parse_7zip_output_version(&out)
        }
        "mingit" => {
            let exe = find_exe_recursive(dir, "git.exe")?;
            let out = run_version_cmd(&exe, &["--version"])?;
            parse_git_semver(&out)
        }
        "nodejs" => {
            let exe = find_exe_recursive(dir, "node.exe")?;
            let out = run_version_cmd(&exe, &["-v"])?;
            extract_first_semver(&out)
        }
        _ => None,
    }
}

fn run_version_cmd(exe: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).creation_flags(CREATE_NO_WINDOW);
    let out = cmd.output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}\n{stderr}");
    if combined.trim().is_empty() {
        None
    } else {
        Some(combined)
    }
}

fn dir_has_files(dir: &Path) -> bool {
    WalkDir::new(dir)
        .max_depth(3)
        .into_iter()
        .flatten()
        .any(|e| {
            e.file_type().is_file()
                && e.file_name().to_str() != Some(".aide_version")
        })
}

fn read_version_receipt(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(".aide_version"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_builtin_remote_or_fallback(key: &str) -> Result<(String, Option<String>)> {
    if let Some((ver, url)) = resolve_remote_info(key) {
        return Ok((url, Some(ver)));
    }
    let fb = find_builtin_fallback(key);
    if !fb.is_empty() {
        return Ok((fb.to_string(), None));
    }
    anyhow::bail!(
        "无法解析内置组件 `{key}` 的下载地址，请检查代理或传入 `--url <直链>`。"
    )
}

/// 查询内置组件的远程 (最新版本号, 下载直链)
fn resolve_remote_info(key: &str) -> Option<(String, String)> {
    match key {
        "nodejs" => resolve_latest_nodejs_lts().ok(),
        "uv" => resolve_latest_uv().ok(),
        "mingit" => resolve_latest_mingit().ok(),
        "7zip" => resolve_latest_7zip().ok(),
        "innounp" => resolve_latest_innounp().ok(),
        "innoextract" => Some((
            "1.9".to_string(),
            find_builtin_fallback("innoextract").to_string(),
        )),
        _ => None,
    }
}

fn find_builtin_fallback(name: &str) -> &'static str {
    BUILTIN_PORTABLE_TOOLS
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.fallback_url)
        .unwrap_or("")
}

fn resolve_latest_nodejs_lts() -> Result<(String, String)> {
    let raw = http_get_text("https://nodejs.org/dist/index.json", 10)?;
    let arr: Vec<Value> = serde_json::from_str(&raw)?;
    for item in arr {
        let is_lts = item
            .get("lts")
            .map(|v| !v.is_boolean() || v.as_bool() == Some(true))
            .unwrap_or(false);
        if is_lts {
            if let Some(ver) = item.get("version").and_then(|v| v.as_str()) {
                let clean = ver.trim_start_matches('v').to_string();
                let url = format!("https://nodejs.org/dist/{ver}/node-{ver}-win-x64.zip");
                return Ok((clean, url));
            }
        }
    }
    anyhow::bail!("未能解析到 Node.js LTS 版本")
}

fn resolve_latest_uv() -> Result<(String, String)> {
    if let Ok((tag, assets)) = query_github_release_json("astral-sh/uv") {
        let ver = tag.trim_start_matches('v').to_string();
        let url = assets
            .into_iter()
            .find(|(n, _)| n == "uv-x86_64-pc-windows-msvc.zip")
            .map(|(_, u)| u)
            .unwrap_or_else(|| {
                format!(
                    "https://github.com/astral-sh/uv/releases/download/{tag}/uv-x86_64-pc-windows-msvc.zip"
                )
            });
        return Ok((ver, url));
    }
    let tag = resolve_github_redirect_tag("astral-sh/uv")?;
    let ver = tag.trim_start_matches('v').to_string();
    let url = format!(
        "https://github.com/astral-sh/uv/releases/download/{tag}/uv-x86_64-pc-windows-msvc.zip"
    );
    Ok((ver, url))
}

fn resolve_latest_mingit() -> Result<(String, String)> {
    if let Ok((tag, assets)) = query_github_release_json("git-for-windows/git") {
        let ver = parse_git_semver(&tag).unwrap_or_else(|| tag.trim_start_matches('v').to_string());
        if let Some((_, url)) = assets.into_iter().find(|(name, _)| {
            name.starts_with("MinGit-") && name.ends_with("-64-bit.zip") && !name.contains("busybox")
        }) {
            return Ok((ver, url));
        }
    }
    let tag = resolve_github_redirect_tag("git-for-windows/git")?;
    let base_ver = extract_first_semver(&tag).context("解析 MinGit 重定向版本失败")?;
    let full_ver = parse_git_semver(&tag).unwrap_or_else(|| base_ver.clone());
    let url = format!(
        "https://github.com/git-for-windows/git/releases/download/{tag}/MinGit-{base_ver}-64-bit.zip"
    );
    Ok((full_ver, url))
}

fn resolve_latest_7zip() -> Result<(String, String)> {
    if let Ok(html) = http_get_text("https://www.7-zip.org/download.html", 10) {
        if let Some(parsed) = parse_7zip_version_from_html(&html) {
            return Ok(parsed);
        }
    }
    if let Ok((tag, assets)) = query_github_release_json("ip7z/7zip") {
        let ver = tag.trim_start_matches('v').to_string();
        let url = assets
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("7zr.exe"))
            .map(|(_, u)| u)
            .unwrap_or_else(|| format!("https://github.com/ip7z/7zip/releases/download/{tag}/7zr.exe"));
        return Ok((ver, url));
    }
    let tag = resolve_github_redirect_tag("ip7z/7zip")?;
    let ver = tag.trim_start_matches('v').to_string();
    Ok((
        ver.clone(),
        format!("https://github.com/ip7z/7zip/releases/download/{tag}/7zr.exe"),
    ))
}

fn resolve_latest_innounp() -> Result<(String, String)> {
    let url = "https://raw.githubusercontent.com/jrathlev/InnoUnpacker-Windows-GUI/refs/heads/master/innounp-2/bin/innounp-2.zip";
    let api = "https://api.github.com/repos/jrathlev/InnoUnpacker-Windows-GUI/commits?path=innounp-2/bin/innounp-2.zip&per_page=1";
    let raw = http_get_text(api, 10)?;
    let arr: Vec<Value> = serde_json::from_str(&raw)?;
    let msg = arr
        .first()
        .and_then(|c| c.get("commit"))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let ver = extract_first_semver(msg).context("未能从提交日志解析 innounp-2 版本")?;
    Ok((ver, url.to_string()))
}

fn query_github_release_json(owner_repo: &str) -> Result<(String, Vec<(String, String)>)> {
    let api = format!("https://api.github.com/repos/{owner_repo}/releases/latest");
    let raw = http_get_text(&api, 10)?;
    let root: Value = serde_json::from_str(&raw)?;
    let tag = root
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("缺少 tag_name")?
        .to_string();
    let mut assets = Vec::new();
    if let Some(arr) = root.get("assets").and_then(|v| v.as_array()) {
        for a in arr {
            if let (Some(n), Some(u)) = (
                a.get("name").and_then(|v| v.as_str()),
                a.get("browser_download_url").and_then(|v| v.as_str()),
            ) {
                assets.push((n.to_string(), u.to_string()));
            }
        }
    }
    Ok((tag, assets))
}

fn resolve_github_redirect_tag(owner_repo: &str) -> Result<String> {
    let url = format!("https://github.com/{owner_repo}/releases/latest");
    let out = Command::new("curl.exe")
        .args([
            "-fsSLI",
            "--connect-timeout",
            "8",
            "--max-time",
            "10",
            "-o",
            "NUL",
            "-w",
            "%{url_effective}",
            &url,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    let final_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let tag = final_url
        .rsplit("/releases/tag/")
        .next()
        .filter(|s| !s.is_empty() && *s != final_url)
        .context("未能解析 GitHub 重定向版本标签")?;
    Ok(tag.trim_end_matches('/').to_string())
}

fn extract_first_semver(text: &str) -> Option<String> {
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || (ch == '.' && !cur.is_empty()) {
            cur.push(ch);
        } else if !cur.is_empty() {
            let trimmed = cur.trim_end_matches('.');
            if trimmed.contains('.') {
                return Some(trimmed.to_string());
            }
            cur.clear();
        }
    }
    let trimmed = cur.trim_end_matches('.');
    if trimmed.contains('.') {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn parse_innounp_version(text: &str) -> Option<String> {
    let idx = text.to_ascii_lowercase().find("version ")?;
    extract_first_semver(&text[idx + 8..])
}

fn parse_7zip_output_version(text: &str) -> Option<String> {
    for line in text.lines() {
        if line.contains("7-Zip") {
            if let Some(v) = extract_first_semver(line) {
                return Some(v);
            }
        }
    }
    extract_first_semver(text)
}

fn parse_7zip_version_from_html(html: &str) -> Option<(String, String)> {
    let marker = "Download 7-Zip ";
    let idx = html.find(marker)?;
    let ver = extract_first_semver(&html[idx + marker.len()..])?;
    let url = if let Some(href_pos) = html.find("7zr.exe\"") {
        let head = &html[..href_pos + 7];
        if let Some(q) = head.rfind('"') {
            let raw_link = &head[q + 1..];
            if raw_link.starts_with("http") {
                raw_link.to_string()
            } else {
                format!("https://www.7-zip.org/{}", raw_link.trim_start_matches('/'))
            }
        } else {
            format!("https://github.com/ip7z/7zip/releases/download/{ver}/7zr.exe")
        }
    } else {
        format!("https://github.com/ip7z/7zip/releases/download/{ver}/7zr.exe")
    };
    Some((ver, url))
}

fn parse_git_semver(text: &str) -> Option<String> {
    let base = extract_first_semver(text)?;
    let lower = text.to_ascii_lowercase();
    if let Some(w_idx) = lower.find(".windows.") {
        let patch: String = text[w_idx + 9..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !patch.is_empty() {
            return Some(format!("{base}.{patch}"));
        }
    }
    Some(base)
}

/// 确保 innounp.exe (innounp-2) 可用，支持 Inno Setup 6.4+ 安装包解包
pub fn ensure_innounp(paths: &PortablePaths) -> Result<PathBuf> {
    ensure_unpacker_tool(
        paths,
        "innounp",
        "innounp.exe",
        &[
            "https://raw.githubusercontent.com/jrathlev/InnoUnpacker-Windows-GUI/refs/heads/master/innounp-2/bin/innounp-2.zip",
        ],
    )
}

/// 确保 innoextract.exe 可用；作为 innounp-2 的兼容回退方案
pub fn ensure_innoextract(paths: &PortablePaths) -> Result<PathBuf> {
    ensure_unpacker_tool(
        paths,
        "innoextract",
        "innoextract.exe",
        &[
            "https://constexpr.org/innoextract/files/innoextract-1.9/innoextract-1.9-windows.zip",
            "https://github.com/dscharrer/innoextract/releases/download/1.9/innoextract-1.9-windows.zip",
        ],
    )
}

/// 查找或自动下载指定名称的便携解包工具
fn ensure_unpacker_tool(
    paths: &PortablePaths,
    tool_dir_name: &str,
    exe_name: &str,
    urls: &[&str],
) -> Result<PathBuf> {
    let candidates = [
        paths.portable_tools_dir.join(tool_dir_name).join(exe_name),
        paths.tools_dir.join(exe_name),
        paths
            .scoop_dir
            .join("apps")
            .join(tool_dir_name)
            .join("current")
            .join(exe_name),
    ];
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }

    println!("[*] 未检测到 {exe_name}，正在自动获取零注册表官方解包工具...");
    let target_dir = paths.portable_tools_dir.join(tool_dir_name);
    std::fs::create_dir_all(&target_dir)?;
    let zip_path = paths.temp_dir.join(format!("{tool_dir_name}-windows.zip"));
    std::fs::create_dir_all(&paths.temp_dir)?;

    let mut downloaded = false;
    for url in urls {
        if download_file(url, &zip_path, true).is_ok() {
            downloaded = true;
            break;
        }
    }
    if !downloaded {
        anyhow::bail!(
            "自动下载 {tool_dir_name} 失败，请检查代理或手动将 {exe_name} 放入 tools/portable/{tool_dir_name}/"
        );
    }

    extract_archive_clean(&zip_path, &target_dir, paths, true)?;
    let _ = std::fs::remove_file(&zip_path);
    let exe = find_exe_recursive(&target_dir, exe_name)
        .with_context(|| format!("解压后未找到 {exe_name}"))?;
    println!("[√] {tool_dir_name} 就绪: {}", paths.rel_display(&exe));
    Ok(exe)
}

fn find_exe_recursive(dir: &Path, filename: &str) -> Option<PathBuf> {
    for entry in WalkDir::new(dir).into_iter().flatten() {
        if entry.file_type().is_file()
            && entry
                .file_name()
                .to_str()
                .map(|n| n.eq_ignore_ascii_case(filename))
                .unwrap_or(false)
        {
            return Some(entry.into_path());
        }
    }
    None
}

fn run_scoop_passthrough(
    scoop_args: &[String],
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    proxy: Option<&ProxyEndpoint>,
) -> Result<()> {
    let scoop_cmd = paths.scoop_dir.join("shims").join("scoop.cmd");
    if !scoop_cmd.is_file() {
        anyhow::bail!("当前目录未部署历史 tools/scoop，请直接使用零软链接便携组件管理。");
    }
    let _ = auto_heal_scoop_if_needed(paths, false);
    let env_vars = build_isolated_env(paths, cfg, proxy);
    let mut cmd = Command::new("cmd.exe");
    cmd.arg("/C").arg(&scoop_cmd).args(scoop_args);
    apply_env_to_command(&mut cmd, &env_vars);
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("命令退出码: {:?}", status.code());
    }
    Ok(())
}

fn parse_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}

fn print_tools_help() {
    println!("用法:");
    println!("  AideRun.exe tools list                               查看已安装原生便携组件及版本");
    println!("  AideRun.exe tools install <innounp|uv|7zip|mingit|nodejs> [--url <URL>]");
    println!("  AideRun.exe tools upgrade <组件名>                   升级已安装组件到最新版本");
    println!("  AideRun.exe tools add-custom <名称> --url <URL> [--bin <子目录>]");
    println!("  AideRun.exe tools remove <名称>                      卸载原生便携组件");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_component_version_outputs() {
        let innounp_out = "\ninnounp - the Inno Setup Unpacker, version 2.71.3 (2026-10-31)\nUsage:";
        assert_eq!(parse_innounp_version(innounp_out).as_deref(), Some("2.71.3"));

        let git_local = "git version 2.56.0.windows.1";
        let git_tag = "v2.56.0.windows.2";
        assert_eq!(parse_git_semver(git_local).as_deref(), Some("2.56.0.1"));
        assert_eq!(parse_git_semver(git_tag).as_deref(), Some("2.56.0.2"));
        assert_eq!(
            compare_semver("2.56.0.2", "2.56.0.1"),
            Ordering::Greater
        );

        let seven_html = r#"<P><B>Download 7-Zip 26.06 (2026-11-06) for Windows</B>:</P><A href="https://github.com/ip7z/7zip/releases/download/26.06/7zr.exe">Download</A>"#;
        let (v, u) = parse_7zip_version_from_html(seven_html).unwrap();
        assert_eq!(v, "26.06");
        assert!(u.ends_with("/26.06/7zr.exe"));
    }
}
