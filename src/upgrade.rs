// AideRun - 官方新版检测（antigravity.google/releases & /download）与零注册表原子升级模块

use std::cmp::Ordering;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::Local;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{LauncherConfig, UpgradeConfig};
use crate::console::{prompt_confirm, CREATE_NO_WINDOW};
use crate::net::{download_file, extract_archive_clean, http_get_text, http_head_ok};
use crate::paths::PortablePaths;
use crate::process::find_blocking_ide_processes;
use crate::toolbox::{ensure_innoextract, ensure_innounp};

/// 官方发布元数据
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ReleaseBuild {
    pub version: String,
    pub execution_id: String,
}

/// 版本检查结果
#[derive(Debug, Clone)]
pub struct UpdateCheckResult {
    pub local_ide_version: Option<String>,
    pub local_vscode_version: Option<String>,
    pub remote_version: String,
    pub remote_execution_id: String,
    pub download_url: String,
    pub source_label: String,
    pub has_update: bool,
}

/// 升级时绝对受保护、绝不覆盖或删除的根目录资产名（小写比较）
const PROTECTED_ROOT_NAMES: &[&str] = &[
    "aiderun.exe",
    "data",
    "projects",
    "tools",
    "logs",
    "version.dll",
    "config.json",
    "start-portable.vbs",
    "antigravity-portable.bat",
    "manage-tools.bat",
    "clean-cache.bat",
    "clean-account-for-packaging.bat",
    "debug.log",
];

/// 解包官方安装包后自动剔除的卸载垃圾文件名（小写）
const IGNORED_INSTALLER_JUNK: &[&str] = &["unins000.exe", "unins000.dat", "$pluginsdir"];

/// 读取当前目录已安装的 Antigravity IDE 版本（ideVersion 与 VSCode 底层版本）
pub fn read_local_versions(paths: &PortablePaths) -> (Option<String>, Option<String>) {
    let product_json = paths.resources_dir.join("app").join("product.json");
    let package_json = paths.resources_dir.join("app").join("package.json");

    let ide_ver = std::fs::read_to_string(&product_json)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim_start_matches('\u{FEFF}')).ok())
        .and_then(|v| v.get("ideVersion").and_then(|x| x.as_str()).map(str::to_string));

    let vscode_ver = std::fs::read_to_string(&package_json)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim_start_matches('\u{FEFF}')).ok())
        .and_then(|v| v.get("version").and_then(|x| x.as_str()).map(str::to_string));

    (ide_ver, vscode_ver)
}

/// 多重容错检测官方最新 Antigravity IDE 版本（对接官方更新 API + /download + /releases 页面）
pub fn check_latest_release(paths: &PortablePaths, cfg: &UpgradeConfig) -> Result<UpdateCheckResult> {
    let (local_ide, local_vscode) = read_local_versions(paths);

    // 1. 优先请求官方 antigravity.google/download 所使用的原生 JSON 发布接口（仅校验 Win-x64 安装包可达性）
    let mut detected: Option<(ReleaseBuild, String, String)> = None;
    if let Ok(json_text) = http_get_text(&cfg.releases_api_url, 12) {
        if let Some((best, url)) = pick_latest_win_x64_build(&json_text) {
            detected = Some((
                best,
                url,
                format!("官方发布接口 Win-x64 ({})", cfg.releases_api_url),
            ));
        }
    }

    // 2. 回退解析 https://antigravity.google/download 页面（含动态 API 发现与 SSR 直链提取）
    if detected.is_none() {
        if let Ok(html) = http_get_text(&cfg.download_page_url, 15) {
            if let Some(found) = parse_download_page_html(&html) {
                detected = Some(found);
            }
        }
    }

    // 3. 回退解析 https://antigravity.google/releases 页面内嵌历史版本 JSON 数组
    if detected.is_none() {
        if let Ok(html) = http_get_text(&cfg.releases_page_url, 15) {
            if let Some(found) = parse_releases_page_html(&html) {
                detected = Some(found);
            }
        }
    }

    let (best, download_url, source_label) = detected.context(
        "无法从 antigravity.google/download 与 /releases 获取版本信息，请检查网络或代理连接。",
    )?;

    let has_update = match &local_ide {
        Some(cur) => compare_semver(&best.version, cur) == Ordering::Greater,
        None => true,
    };

    Ok(UpdateCheckResult {
        local_ide_version: local_ide,
        local_vscode_version: local_vscode,
        remote_version: best.version,
        remote_execution_id: best.execution_id,
        download_url,
        source_label,
        has_update,
    })
}

/// 处理 `check-update` 命令
pub fn handle_check_update(paths: &PortablePaths, cfg: &LauncherConfig) -> Result<()> {
    println!("[*] 正在检测 Antigravity IDE 官方最新版本...");
    let res = check_latest_release(paths, &cfg.upgrade)?;
    println!(
        "  当前本地版本 : {}",
        res.local_ide_version.as_deref().unwrap_or("未安装/未检测到")
    );
    if let Some(vs) = &res.local_vscode_version {
        println!("  底层内核版本 : VS Code {vs}");
    }
    println!(
        "  官方最新版本 : {} (Build {})",
        res.remote_version, res.remote_execution_id
    );
    println!("  检测数据来源 : {}", res.source_label);
    println!("  官方直链地址 : {}", res.download_url);

    if res.has_update {
        println!(
            "\n[!] 发现可用更新！可运行 `AideRun.exe upgrade` 自动无痕升级，或自行下载后运行 `AideRun.exe upgrade <安装包路径>`。"
        );
    } else {
        println!("\n[√] 当前已是官方最新稳定版。");
    }
    Ok(())
}

/// 升级结果摘要
#[derive(Debug, Clone)]
pub struct UpgradeOutcome {
    pub old_version: Option<String>,
    pub new_version: Option<String>,
    pub up_to_date: bool,
    pub cancelled: bool,
}

/// 处理 `upgrade` 命令（支持在线自动升级、本地单文件安装包、URL 直传、--rollback、--clean-backup）
pub fn handle_upgrade_command(
    args: &[String],
    paths: &PortablePaths,
    cfg: &LauncherConfig,
) -> Result<()> {
    if args.iter().any(|a| a == "--check") {
        return handle_check_update(paths, cfg);
    }
    if args.iter().any(|a| a == "--rollback") {
        return rollback_upgrade(paths);
    }
    if args.iter().any(|a| a == "--clean-backup") {
        return clean_upgrade_backups(paths);
    }

    let auto_yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let source_arg = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);
    let _ = execute_upgrade(source_arg, auto_yes, paths, cfg)?;
    Ok(())
}

/// 执行无痕升级主流程（供 CLI 与图形面板复用）
pub fn execute_upgrade(
    source_arg: Option<&str>,
    auto_yes: bool,
    paths: &PortablePaths,
    cfg: &LauncherConfig,
) -> Result<UpgradeOutcome> {
    let blockers = find_blocking_ide_processes(&paths.root);
    if !blockers.is_empty() {
        println!("[错误] 检测到当前便携目录内仍有进程运行，升级前请先关闭 Antigravity IDE：");
        for p in &blockers {
            println!("  - PID {:<6} {}", p.pid, p.name);
        }
        anyhow::bail!("检测到 Antigravity IDE 相关进程正在运行，请先关闭后再升级。");
    }

    let stage_root = std::env::temp_dir().join(format!("AideRun_upgrade_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage_root);
    std::fs::create_dir_all(&stage_root)?;

    let res = (|| -> Result<UpgradeOutcome> {
        let package_file = match source_arg {
            Some(src) if src.starts_with("http://") || src.starts_with("https://") => {
                let dst = stage_root.join("Antigravity_IDE_installer.exe");
                println!("[*] 正在下载官方安装包: {src}");
                download_file(src, &dst, true)?;
                dst
            }
            Some(local_path) => {
                let p = PathBuf::from(local_path);
                if !p.is_file() {
                    anyhow::bail!("找不到指定的安装包文件: {}", p.display());
                }
                p
            }
            None => {
                let check = check_latest_release(paths, &cfg.upgrade)?;
                println!(
                    "本地版本: {} -> 官方最新: {}",
                    check.local_ide_version.as_deref().unwrap_or("未知"),
                    check.remote_version
                );
                if !check.has_update && paths.ide_exe.is_file() {
                    println!("[√] 当前已经是最新版本，无需升级。");
                    return Ok(UpgradeOutcome {
                        old_version: check.local_ide_version,
                        new_version: Some(check.remote_version),
                        up_to_date: true,
                        cancelled: false,
                    });
                }
                println!("官方安装包地址: {}", check.download_url);
                if !auto_yes && !prompt_confirm("是否自动下载并无痕升级（零注册表）？[y/N]: ", false)
                {
                    println!("[已取消升级]");
                    return Ok(UpgradeOutcome {
                        old_version: check.local_ide_version,
                        new_version: None,
                        up_to_date: false,
                        cancelled: true,
                    });
                }
                let dst = stage_root.join("Antigravity_IDE_installer.exe");
                println!("[*] 正在下载官方最新版本...");
                download_file(&check.download_url, &dst, true)?;
                dst
            }
        };

        let extract_dir = stage_root.join("extracted");
        std::fs::create_dir_all(&extract_dir)?;

        let payload_dir = unpack_ide_package(&package_file, &extract_dir, paths)?;
        let new_product = payload_dir.join("resources").join("app").join("product.json");
        let new_ver = std::fs::read_to_string(&new_product)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("ideVersion").and_then(|x| x.as_str()).map(str::to_string));

        let (old_ver, _) = read_local_versions(paths);
        println!(
            "[*] 内核校验通过！准备替换：{} -> {}",
            old_ver.as_deref().unwrap_or("未知"),
            new_ver.as_deref().unwrap_or("新版")
        );

        apply_atomic_core_swap(&payload_dir, paths)?;
        println!("[√] Antigravity IDE 无痕升级完成！（已保留 data/、projects/、tools/、version.dll 与 config.json）");
        println!("    若新版本启动异常，可执行 `AideRun.exe upgrade --rollback` 秒级回退旧版。");
        Ok(UpgradeOutcome {
            old_version: old_ver,
            new_version: new_ver,
            up_to_date: false,
            cancelled: false,
        })
    })();

    let _ = std::fs::remove_dir_all(&stage_root);
    res
}

/// 将外部下载或本地单文件安装包解包并定位至含 `Antigravity IDE.exe` 与 `resources/` 的载荷根目录
pub fn unpack_ide_package(
    package_file: &Path,
    extract_dir: &Path,
    paths: &PortablePaths,
) -> Result<PathBuf> {
    let ext = package_file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "exe" {
        if is_inno_setup_installer(package_file) {
            unpack_inno_setup_installer(package_file, extract_dir, paths)?;
        } else {
            // 非 Inno Setup 的 exe（如 7z 自解压/NSIS）尝试用 7z 解包；严禁直接把未知 exe 覆盖到主程序
            println!("[*] 尝试通过归档解包器提取安装包内容...");
            extract_archive_clean(package_file, extract_dir, paths, false).context(
                "该 .exe 不是标准的 Inno Setup 或可解包归档格式；为防止安装包覆盖主程序，已安全终止！",
            )?;
        }
    } else {
        extract_archive_clean(package_file, extract_dir, paths, false)?;
    }

    find_extracted_ide_root(extract_dir).context(
        "解包完成但未找到完整的 Antigravity IDE.exe 与 resources/ 目录结构，已取消替换。",
    )
}

/// 构造 innounp-2 的 `-d<DIR>` 目标参数（注意 `-d` 与路径之间不能有空格）
pub fn format_innounp_dest_arg(extract_dir: &Path) -> String {
    format!("-d{}", extract_dir.display())
}

/// 零注册表解包 Inno Setup 安装包：优先使用支持 6.4+ 的 innounp-2，失败时自动回退 innoextract / 7z
fn unpack_inno_setup_installer(
    package_file: &Path,
    extract_dir: &Path,
    paths: &PortablePaths,
) -> Result<()> {
    println!("[*] 检测到 Inno Setup 单文件安装包，正在执行零注册表静默解包...");
    let mut errors = Vec::<String>::new();

    match ensure_innounp(paths) {
        Ok(innounp) => {
            let dest_arg = format_innounp_dest_arg(extract_dir);
            match Command::new(&innounp)
                .args(["-x", "-b", "-q", "-y", &dest_arg])
                .arg(package_file)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
            {
                Ok(out) if out.status.success() && find_extracted_ide_root(extract_dir).is_some() => {
                    return Ok(());
                }
                Ok(out) => {
                    let msg = format!(
                        "{} {}",
                        String::from_utf8_lossy(&out.stdout).trim(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                    errors.push(format!("innounp-2 ({:?}): {}", out.status.code(), msg.trim()));
                }
                Err(e) => errors.push(format!("启动 innounp.exe 失败: {e}")),
            }
        }
        Err(e) => errors.push(format!("获取 innounp-2 失败: {e:#}")),
    }

    // 清理未完整解包的临时目录后尝试兼容解包器
    let _ = std::fs::remove_dir_all(extract_dir);
    let _ = std::fs::create_dir_all(extract_dir);

    if let Ok(innoextract) = ensure_innoextract(paths) {
        if let Ok(out) = Command::new(&innoextract)
            .args(["-e", "-s", "-d"])
            .arg(extract_dir)
            .arg(package_file)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if out.status.success() && find_extracted_ide_root(extract_dir).is_some() {
                return Ok(());
            }
            errors.push(format!(
                "innoextract: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    let _ = std::fs::remove_dir_all(extract_dir);
    let _ = std::fs::create_dir_all(extract_dir);
    if extract_archive_clean(package_file, extract_dir, paths, false).is_ok()
        && find_extracted_ide_root(extract_dir).is_some()
    {
        return Ok(());
    }

    anyhow::bail!("Inno Setup 安装包解包失败：{}", errors.join(" | "))
}

/// 扫描文件前 4MB 字节流判断是否为 Inno Setup 安装包（防止误把官方同名安装向导当成主程序覆盖）
pub fn is_inno_setup_installer(exe_path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(exe_path) else {
        return false;
    };
    const MAX_SCAN: usize = 4 * 1024 * 1024;
    const CHUNK: usize = 64 * 1024;
    let needle = b"Inno Setup";
    let mut total_read = 0usize;
    let mut tail = Vec::<u8>::new();

    while total_read < MAX_SCAN {
        let mut buf = vec![0u8; CHUNK];
        let Ok(n) = f.read(&mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        buf.truncate(n);
        let mut window = Vec::with_capacity(tail.len() + n);
        window.extend_from_slice(&tail);
        window.extend_from_slice(&buf);
        if window.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        let keep = needle.len().saturating_sub(1);
        tail = window[window.len().saturating_sub(keep)..].to_vec();
        total_read += n;
    }
    false
}

/// 在解包目录中定位同时存在 `Antigravity IDE.exe` 与 `resources/` 的 `{app}` 根目录
pub fn find_extracted_ide_root(extract_dir: &Path) -> Option<PathBuf> {
    if is_valid_ide_payload_dir(extract_dir) {
        return Some(extract_dir.to_path_buf());
    }
    for sub in ["app", "{app}"] {
        let candidate = extract_dir.join(sub);
        if is_valid_ide_payload_dir(&candidate) {
            return Some(candidate);
        }
    }
    for entry in walkdir::WalkDir::new(extract_dir)
        .max_depth(3)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_dir() && is_valid_ide_payload_dir(entry.path()) {
            return Some(entry.into_path());
        }
    }
    None
}

fn is_valid_ide_payload_dir(dir: &Path) -> bool {
    dir.join("Antigravity IDE.exe").is_file() && dir.join("resources").is_dir()
}

/// 白名单保护替换 IDE 内核：先将旧内核备份至 `data/_upgrade_backup/<ts>/`，再部署新内核
pub fn apply_atomic_core_swap(new_payload_dir: &Path, paths: &PortablePaths) -> Result<()> {
    let ts = Local::now().format("%Y%m%d_%H%M%S").to_string();
    let backup_dir = paths.upgrade_backup_dir.join(&ts);
    std::fs::create_dir_all(&backup_dir)?;

    // 1. 仅迁移将被新内核覆盖或属于旧 IDE 核心的文件至回滚备份目录
    for entry in std::fs::read_dir(&paths.root)?.flatten() {
        let existing = entry.path();
        let Some(name_str) = existing.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_protected_root_item(name_str) {
            continue;
        }
        let is_core_item = new_payload_dir.join(name_str).exists() || is_known_ide_core_item(name_str);
        if is_core_item {
            let dst = backup_dir.join(name_str);
            move_or_copy_replace(&existing, &dst)?;
        }
    }

    // 2. 将新内核文件部署到根目录（严格跳过受保护项与卸载残留）
    for entry in std::fs::read_dir(new_payload_dir)?.flatten() {
        let src = entry.path();
        let Some(name_str) = src.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_protected_root_item(name_str) || is_ignored_installer_junk(name_str) {
            continue;
        }
        let dst = paths.root.join(name_str);
        if let Err(err) = move_or_copy_replace(&src, &dst) {
            // 异常时立刻自动回滚旧版
            let _ = restore_backup_dir(&backup_dir, paths);
            return Err(err).context("写入新版内核失败，已自动回滚到原版本");
        }
    }

    Ok(())
}

/// 回滚至最近一次升级保留的旧内核快照
pub fn rollback_upgrade(paths: &PortablePaths) -> Result<()> {
    let blockers = find_blocking_ide_processes(&paths.root);
    if !blockers.is_empty() {
        anyhow::bail!("检测到 Antigravity IDE 仍在运行，请先关闭后再执行回滚。");
    }
    let mut snapshots: Vec<PathBuf> = std::fs::read_dir(&paths.upgrade_backup_dir)
        .context("未找到任何可供回滚的升级备份记录 (data/_upgrade_backup)")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    snapshots.sort();
    let latest = snapshots
        .pop()
        .context("升级备份目录为空，无版本可回滚。")?;

    restore_backup_dir(&latest, paths)?;
    println!("[√] 已成功回滚到升级前快照: {}", paths.rel_display(&latest));
    Ok(())
}

fn restore_backup_dir(backup_dir: &Path, paths: &PortablePaths) -> Result<()> {
    for entry in std::fs::read_dir(backup_dir)?.flatten() {
        let src = entry.path();
        let Some(name_str) = src.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_protected_root_item(name_str) {
            continue;
        }
        let dst = paths.root.join(name_str);
        if dst.exists() {
            if dst.is_dir() {
                let _ = std::fs::remove_dir_all(&dst);
            } else {
                let _ = std::fs::remove_file(&dst);
            }
        }
        move_or_copy_replace(&src, &dst)?;
    }
    let _ = std::fs::remove_dir_all(backup_dir);
    Ok(())
}

fn clean_upgrade_backups(paths: &PortablePaths) -> Result<()> {
    if paths.upgrade_backup_dir.is_dir() {
        std::fs::remove_dir_all(&paths.upgrade_backup_dir)?;
        println!("[√] 已清理全部历史升级回滚快照 (data/_upgrade_backup)。");
    } else {
        println!("[-] 当前无升级备份快照需要清理。");
    }
    Ok(())
}

/// 判断根目录条目是否属于用户/启动器/代理受保护资产
pub fn is_protected_root_item(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    PROTECTED_ROOT_NAMES.contains(&lower.as_str())
}

fn is_ignored_installer_junk(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    IGNORED_INSTALLER_JUNK.contains(&lower.as_str())
}

fn is_known_ide_core_item(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "antigravity ide.exe"
            | "antigravity ide.visualelementsmanifest.xml"
            | "appx"
            | "bin"
            | "locales"
            | "resources"
            | "chrome_100_percent.pak"
            | "chrome_200_percent.pak"
            | "resources.pak"
            | "snapshot_blob.bin"
            | "v8_context_snapshot.bin"
            | "icudtl.dat"
            | "licenses.chromium.html"
            | "vk_swiftshader_icd.json"
            | "d3dcompiler_47.dll"
            | "dxcompiler.dll"
            | "dxil.dll"
            | "ffmpeg.dll"
            | "libegl.dll"
            | "libglesv2.dll"
            | "vk_swiftshader.dll"
            | "vulkan-1.dll"
    )
}

fn move_or_copy_replace(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        if dst.is_dir() {
            std::fs::remove_dir_all(dst)?;
        } else {
            std::fs::remove_file(dst)?;
        }
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    if src.is_dir() {
        copy_dir_recursive(src, dst)?;
        let _ = std::fs::remove_dir_all(src);
    } else {
        std::fs::copy(src, dst)?;
        let _ = std::fs::remove_file(src);
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if p.is_dir() {
            copy_dir_recursive(&p, &target)?;
        } else {
            std::fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

/// 构造官方 Antigravity IDE Windows x64 下载 URL
pub fn build_ide_win_x64_url(version: &str, execution_id: &str) -> String {
    format!(
        "https://edgedl.me.gvt1.com/edgedl/release2/j0qc3/antigravity/stable/{version}-{execution_id}/windows-x64/Antigravity%20IDE.exe"
    )
}

/// 解析官方 JSON 数组并选出语义版本最高的条目（单元测试用）
#[cfg(test)]
pub fn parse_releases_json(raw: &str) -> Option<ReleaseBuild> {
    parse_releases_json_sorted(raw).into_iter().next()
}

/// 解析官方 JSON 数组并按语义版本降序排列
fn parse_releases_json_sorted(raw: &str) -> Vec<ReleaseBuild> {
    let mut list: Vec<ReleaseBuild> = serde_json::from_str(raw).unwrap_or_default();
    list.retain(|b| !b.version.trim().is_empty() && !b.execution_id.trim().is_empty());
    list.sort_by(|a, b| compare_semver(&b.version, &a.version));
    list
}

/// 仅筛选存在有效 `windows-x64/Antigravity%20IDE.exe` 安装包的版本条目
fn pick_latest_win_x64_build(raw: &str) -> Option<(ReleaseBuild, String)> {
    let list = parse_releases_json_sorted(raw);
    for cand in &list {
        let url = build_ide_win_x64_url(&cand.version, &cand.execution_id);
        if http_head_ok(&url) {
            return Some((cand.clone(), url));
        }
    }
    list.into_iter().next().map(|cand| {
        let url = build_ide_win_x64_url(&cand.version, &cand.execution_id);
        (cand, url)
    })
}

/// 从 `https://antigravity.google/download` HTML 中提取 Win-x64 IDE 版本及下载链接
pub fn parse_download_page_html(html: &str) -> Option<(ReleaseBuild, String, String)> {
    // 1. 优先尝试页面中动态出现的 antigravity-ide-auto-updater 接口地址，并按 Win-x64 校验
    if let Some(api_url) = extract_ide_updater_url(html) {
        if let Ok(json_text) = http_get_text(&api_url, 10) {
            if let Some((best, dl)) = pick_latest_win_x64_build(&json_text) {
                return Some((best, dl, format!("download 页面动态接口 Win-x64 ({api_url})")));
            }
        }
    }

    // 2. 回退提取 HTML 内渲染的 windows-x64/Antigravity%20IDE.exe 直链
    let marker = "edgedl.me.gvt1.com/edgedl/release2/j0qc3/antigravity/stable/";
    let mut best: Option<(ReleaseBuild, String)> = None;
    let mut offset = 0usize;
    while let Some(rel_pos) = html[offset..].find(marker) {
        let start = offset + rel_pos;
        let tail = &html[start + marker.len()..];
        let end = tail
            .find(|c: char| matches!(c, '"' | '\'' | '<' | '>' | ' ' | '\n' | '\r' | '`'))
            .unwrap_or(tail.len());
        let segment = &tail[..end];
        if segment.ends_with("/windows-x64/Antigravity%20IDE.exe") {
            if let Some((ver_exec, _)) = segment.split_once("/windows-x64/") {
                if let Some((ver, exec_id)) = ver_exec.split_once('-') {
                    if !ver.contains('$') && !exec_id.contains('$') {
                        let candidate = ReleaseBuild {
                            version: ver.to_string(),
                            execution_id: exec_id.to_string(),
                        };
                        let replace = best
                            .as_ref()
                            .map(|(cur, _)| compare_semver(&candidate.version, &cur.version) == Ordering::Greater)
                            .unwrap_or(true);
                        if replace {
                            let full = format!("https://{marker}{segment}");
                            best = Some((candidate, full));
                        }
                    }
                }
            }
        }
        offset = start + marker.len();
    }

    best.map(|(b, url)| (b, url, "antigravity.google/download 页面静态链接".to_string()))
}

fn extract_ide_updater_url(html: &str) -> Option<String> {
    let needle = "https://antigravity-ide-auto-updater";
    let start = html.find(needle)?;
    let rest = &html[start..];
    let end = rest.find(|c: char| matches!(c, '`' | '"' | '\'' | '<' | '>'))?;
    Some(rest[..end].to_string())
}

/// 从 `https://antigravity.google/releases` HTML 中提取 Antigravity IDE 版本列表（严格验证非 Hub 版）
pub fn parse_releases_page_html(html: &str) -> Option<(ReleaseBuild, String, String)> {
    let decoded = html.replace("&quot;", "\"");
    let mut candidates = Vec::<ReleaseBuild>::new();

    // Antigravity IDE 发布记录位于包含 `Antigravity%20IDE` 或 `data-ide` 的数据区或第二段版本数组中
    let search_slice = if let Some(idx) = decoded.find("Antigravity%20IDE") {
        &decoded[idx..]
    } else if let Some(idx) = decoded.find("\"1.11.2\"") {
        // IDE 历史列表末尾含 1.11.2 里程碑；向前回溯 4000 字符涵盖最新 IDE 数组
        let start = idx.saturating_sub(4000);
        &decoded[start..idx + 500.min(decoded.len() - idx)]
    } else {
        decoded.as_str()
    };

    let mut pos = 0usize;
    while let Some(idx) = search_slice[pos..].find("{\"version\":\"") {
        let start = pos + idx;
        if let Some(end_rel) = search_slice[start..].find('}') {
            let chunk = &search_slice[start..start + end_rel + 1];
            if let Ok(entry) = serde_json::from_str::<ReleaseBuild>(chunk) {
                candidates.push(entry);
            }
            pos = start + end_rel + 1;
        } else {
            break;
        }
    }

    candidates.sort_by(|a, b| compare_semver(&b.version, &a.version));
    for cand in candidates {
        let url = build_ide_win_x64_url(&cand.version, &cand.execution_id);
        if http_head_ok(&url) {
            return Some((
                cand,
                url,
                "antigravity.google/releases 页面版本校验".to_string(),
            ));
        }
    }
    None
}

/// 语义版本号比较（如 2.10.0 > 2.6.0）
pub fn compare_semver(a: &str, b: &str) -> Ordering {
    let parse_parts = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let pa = parse_parts(a);
    let pb = parse_parts(b);
    let len = pa.len().max(pb.len());
    for i in 0..len {
        let va = *pa.get(i).unwrap_or(&0);
        let vb = *pb.get(i).unwrap_or(&0);
        match va.cmp(&vb) {
            Ordering::Equal => continue,
            ord => return ord,
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_numerically() {
        assert_eq!(compare_semver("2.10.0", "2.6.0"), Ordering::Greater);
        assert_eq!(compare_semver("2.6.0", "2.6.0"), Ordering::Equal);
        assert_eq!(compare_semver("2.5.5", "2.6.0"), Ordering::Less);
    }

    #[test]
    fn parses_official_api_and_download_html() {
        let json = r#"[{"version":"2.5.5","execution_id":"111"},{"version":"2.10.0","execution_id":"999"}]"#;
        let best = parse_releases_json(json).unwrap();
        assert_eq!(best.version, "2.10.0");
        assert_eq!(best.execution_id, "999");

        // 同时包含 macOS / Linux / Windows 链接时，严格仅匹配 windows-x64/Antigravity%20IDE.exe
        let html = r#"
          <a href="https://edgedl.me.gvt1.com/edgedl/release2/j0qc3/antigravity/stable/9.9.9-111/darwin-arm64/Antigravity.dmg">mac</a>
          <a href="https://edgedl.me.gvt1.com/edgedl/release2/j0qc3/antigravity/stable/2.6.0-5526304964083712/windows-x64/Antigravity%20IDE.exe">win</a>
        "#;
        let (b, url, _) = parse_download_page_html(html).unwrap();
        assert_eq!(b.version, "2.6.0");
        assert!(url.ends_with("/windows-x64/Antigravity%20IDE.exe"));
        assert_eq!(
            format_innounp_dest_arg(Path::new(r"D:\Antigravity IDE\tmp")),
            r"-dD:\Antigravity IDE\tmp"
        );
    }

    #[test]
    fn protects_user_assets_during_upgrade_swap() {
        let tmp = std::env::temp_dir().join(format!("aiderun_upg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("root");
        let payload = tmp.join("payload");
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("resources").join("app")).unwrap();
        std::fs::write(root.join("data").join("keep.txt"), b"user_data").unwrap();
        std::fs::write(root.join("version.dll"), b"proxy_dll").unwrap();
        std::fs::write(root.join("config.json"), b"proxy_cfg").unwrap();
        std::fs::write(root.join("Antigravity IDE.exe"), b"old_ide").unwrap();

        std::fs::create_dir_all(payload.join("resources").join("app")).unwrap();
        std::fs::write(payload.join("Antigravity IDE.exe"), b"new_ide").unwrap();
        std::fs::write(payload.join("resources").join("app").join("package.json"), b"{}").unwrap();
        std::fs::write(payload.join("unins000.exe"), b"junk").unwrap();

        let paths = PortablePaths::from_root(root.clone(), root.join("AideRun.exe"));
        apply_atomic_core_swap(&payload, &paths).unwrap();

        assert_eq!(std::fs::read(root.join("Antigravity IDE.exe")).unwrap(), b"new_ide");
        assert_eq!(std::fs::read(root.join("version.dll")).unwrap(), b"proxy_dll");
        assert_eq!(std::fs::read(root.join("config.json")).unwrap(), b"proxy_cfg");
        assert_eq!(std::fs::read(root.join("data").join("keep.txt")).unwrap(), b"user_data");
        assert!(!root.join("unins000.exe").exists());

        rollback_upgrade(&paths).unwrap();
        assert_eq!(std::fs::read(root.join("Antigravity IDE.exe")).unwrap(), b"old_ide");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

