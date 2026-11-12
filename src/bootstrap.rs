// AideRun - 首次纯净目录自动便携环境部署、环境完整性诊断与官方安装包原地解包引擎

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::LauncherConfig;
use crate::console::{init_console, prompt_confirm, ConsoleModeKind};
use crate::env::apply_proxy_and_tools_to_self;
use crate::net::download_file_with_progress;
use crate::paths::PortablePaths;
use crate::proxy::resolve_and_sync_proxy;
use crate::toolbox::install_portable_tool;
use crate::upgrade::{
    apply_atomic_core_swap, check_latest_release, is_inno_setup_installer, read_local_versions,
    unpack_ide_package,
};

/// 当前目录便携开发环境状态检测结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableEnvStatus {
    /// 目录已具备 IDE 可执行文件与 resources/ 核心资源
    ReadyPortable,
    /// 目录仅含待原地解包的官方单文件 Inno Setup 安装包（Antigravity IDE.exe）
    LocalInstallerPendingUnpack,
    /// 缺少 Antigravity IDE 环境（例如当前目录只有 AideRun.exe）
    MissingIdeEnvironment,
}

/// 官网在线部署阶段实时进度信息（供子窗口进度条与日志窗渲染）
#[derive(Debug, Clone)]
pub struct DeployProgress {
    pub percent: u8,
    pub stage: String,
    pub detail: String,
    pub log_line: Option<String>,
}

/// 部署结果摘要
#[derive(Debug, Clone, Default)]
pub struct BootstrapReport {
    pub unpacked_installer: bool,
    pub deployed_skeleton: bool,
    pub seeded_configs: bool,
}

/// 无副作用检测当前目录是否已经具备便携 Antigravity IDE 环境（绝不提前创建目录）
pub fn inspect_portable_env_status(paths: &PortablePaths) -> PortableEnvStatus {
    if paths.ide_exe.is_file() && paths.resources_dir.is_dir() {
        return PortableEnvStatus::ReadyPortable;
    }
    if paths.ide_exe.is_file() && is_inno_setup_installer(&paths.ide_exe) {
        return PortableEnvStatus::LocalInstallerPendingUnpack;
    }
    PortableEnvStatus::MissingIdeEnvironment
}

fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn format_speed(bps: f64) -> String {
    if bps >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bps / (1024.0 * 1024.0))
    } else if bps >= 1024.0 {
        format!("{:.0} KB/s", bps / 1024.0)
    } else {
        "连接中...".to_string()
    }
}

/// 在用户确认后从官网下载最新 Windows x64 安装包并在当前目录零注册表部署完整便携环境
pub fn deploy_portable_ide_from_official<F>(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    save_cfg: bool,
    mut on_progress: F,
) -> Result<String>
where
    F: FnMut(DeployProgress),
{
    on_progress(DeployProgress {
        percent: 4,
        stage: "查询官网最新版本".to_string(),
        detail: "正在请求 antigravity.google 获取 Windows x64 最新稳定版信息...".to_string(),
        log_line: Some("[*] 正在连接官网查询最新 Antigravity IDE 安装包版本...".to_string()),
    });

    let check = check_latest_release(paths, &cfg.upgrade)
        .context("查询官网最新 Antigravity IDE 版本失败，请检查代理连接")?;

    on_progress(DeployProgress {
        percent: 9,
        stage: format!("锁定官网版本 v{}", check.remote_version),
        detail: format!("下载源: {}", check.source_label),
        log_line: Some(format!(
            "[√] 获取官网版本 v{} 成功，准备创建便携目录骨架...",
            check.remote_version
        )),
    });

    // 确认官网元数据可达后再正式在当前目录落盘创建便携目录结构与配置
    paths.ensure_skeleton()?;
    if save_cfg {
        cfg.save(&paths.launcher_config)?;
    }
    let proxy = resolve_and_sync_proxy(&paths.proxy_config, &cfg.proxy);
    apply_proxy_and_tools_to_self(paths, cfg, proxy.as_ref());

    let stage_dir = std::env::temp_dir().join(format!("AideRun_deploy_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage_dir);
    let extract_dir = stage_dir.join("extract");
    std::fs::create_dir_all(&extract_dir)?;
    let installer_file = stage_dir.join("Antigravity-Setup.exe");

    let deploy_res = (|| -> Result<String> {
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        let mut last_logged_bucket: i32 = -1;

        on_progress(DeployProgress {
            percent: 12,
            stage: format!("下载官方安装包 v{}", check.remote_version),
            detail: "正在建立下载连接...".to_string(),
            log_line: Some(format!("[*] 开始下载官方安装包: {}", check.download_url)),
        });

        download_file_with_progress(&check.download_url, &installer_file, |done, total, speed_bps| {
            let now = Instant::now();
            let raw_pct = match total {
                Some(total_bytes) if total_bytes > 0 => {
                    let ratio = (done as f64 / total_bytes as f64).clamp(0.0, 1.0);
                    12 + (ratio * 60.0).round() as u8
                }
                _ => {
                    let est_ratio = (done as f64 / (250.0 * 1024.0 * 1024.0)).min(0.95);
                    12 + (est_ratio * 60.0).round() as u8
                }
            };

            let bucket = match total {
                Some(t) if t > 0 => ((done as f64 / t as f64) * 10.0).floor() as i32,
                _ => (done / (30 * 1024 * 1024)) as i32,
            };
            let should_log = done > 0 && bucket > last_logged_bucket;
            let reached_end = total.is_some_and(|t| t > 0 && done >= t);
            if !should_log && !reached_end && now.duration_since(last_emit) < Duration::from_millis(320) {
                return;
            }
            last_emit = now;

            let detail = match total {
                Some(t) if t > 0 => format!(
                    "已下载 {} / {} ({})",
                    format_mb(done),
                    format_mb(t),
                    format_speed(speed_bps)
                ),
                _ => format!("已下载 {} ({})", format_mb(done), format_speed(speed_bps)),
            };

            let log_line = if should_log {
                last_logged_bucket = bucket;
                Some(format!("[*] 下载进度: {detail}"))
            } else {
                None
            };

            on_progress(DeployProgress {
                percent: raw_pct.min(72),
                stage: format!("下载官方安装包 v{}", check.remote_version),
                detail,
                log_line,
            });
        })
        .context("从官网下载 Antigravity IDE 安装包失败")?;

        on_progress(DeployProgress {
            percent: 76,
            stage: "准备零注册表解包器".to_string(),
            detail: "正在调用便携 innounp 解包官网安装包...".to_string(),
            log_line: Some("[√] 安装包下载完成，正在零注册表提取 IDE 核心文件...".to_string()),
        });

        let payload_dir = unpack_ide_package(&installer_file, &extract_dir, paths)
            .context("解包官方 Antigravity IDE 安装包失败")?;

        on_progress(DeployProgress {
            percent: 88,
            stage: "部署核心文件至当前目录".to_string(),
            detail: format!("目标目录: {}", paths.root.display()),
            log_line: Some("[*] 正在将 Antigravity IDE.exe 与 resources/ 写入当前目录...".to_string()),
        });

        apply_atomic_core_swap(&payload_dir, paths).context("写入便携版核心程序文件失败")?;
        if paths.upgrade_backup_dir.is_dir() {
            let _ = std::fs::remove_dir_all(&paths.upgrade_backup_dir);
        }

        on_progress(DeployProgress {
            percent: 96,
            stage: "写入隔离配置与环境标记".to_string(),
            detail: "正在生成 data/argv.json、settings.json 与 .portable_root...".to_string(),
            log_line: Some("[*] 正在初始化默认便携配置文件与路径锚点...".to_string()),
        });

        seed_default_portable_configs(paths, cfg)?;
        let _ = std::fs::write(&paths.root_marker, format!("{}\n", paths.root.display()));

        let (local_ver, _) = read_local_versions(paths);
        let final_ver = local_ver.unwrap_or_else(|| check.remote_version.clone());

        on_progress(DeployProgress {
            percent: 100,
            stage: "便携开发环境部署完成".to_string(),
            detail: format!("Antigravity IDE v{final_ver} 已就绪，正在进入主控制台..."),
            log_line: Some(format!("[√] 全部部署完成！本地当前版本: v{final_ver}")),
        });

        Ok(final_ver)
    })();

    let _ = std::fs::remove_dir_all(&stage_dir);
    deploy_res
}

/// 自动检测当前目录便携环境完整性；若尚未部署（如仅有 AideRun.exe 与 Antigravity IDE.exe）则自动部署就绪
pub fn ensure_portable_ready(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    interactive_tools_prompt: bool,
) -> Result<BootstrapReport> {
    let mut report = BootstrapReport::default();

    let needs_skeleton = paths.needs_portable_bootstrap();
    let needs_installer_unpack =
        paths.ide_exe.is_file() && !paths.resources_dir.is_dir() && is_inno_setup_installer(&paths.ide_exe);

    if !needs_skeleton && !needs_installer_unpack {
        return Ok(report);
    }

    // 若需要解包 200MB 安装包或显式初始化工具链，自动开启可见进度控制台
    if needs_installer_unpack || interactive_tools_prompt {
        init_console(
            ConsoleModeKind::EnsureInteractive,
            "AideRun - 自动部署 Antigravity IDE 便携环境",
        );
        println!("==================================================================");
        println!("      检测到当前目录尚未完成便携环境部署，正在自动初始化...");
        println!("==================================================================");
    }

    // 1. 先搭建全套隔离目录结构
    if needs_skeleton {
        paths.ensure_skeleton()?;
        report.deployed_skeleton = true;
    }

    // 解析系统代理，确保后续自动解包/下载 innoextract 或便携组件走代理无阻
    let proxy = resolve_and_sync_proxy(&paths.proxy_config, &cfg.proxy);
    apply_proxy_and_tools_to_self(paths, cfg, proxy.as_ref());

    // 2. 若根目录 Antigravity IDE.exe 本身是官方单文件 Inno Setup 安装包，先无痕解包替换为真实 IDE
    if needs_installer_unpack {
        println!("[*] 检测到 Antigravity IDE.exe 为官方单文件安装包（尚无 resources/ 目录）");
        println!("[*] 正在执行零注册表原地解包部署，请稍候...");

        let stage_dir = std::env::temp_dir().join(format!("AideRun_init_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&stage_dir);
        std::fs::create_dir_all(&stage_dir)?;

        let unpack_res = (|| -> Result<()> {
            let payload_dir = unpack_ide_package(&paths.ide_exe, &stage_dir, paths)?;
            // 移除原有安装包文件占位，将解包后的真实内核放入根目录（随后删除临时备份）
            apply_atomic_core_swap(&payload_dir, paths)?;
            if paths.upgrade_backup_dir.is_dir() {
                let _ = std::fs::remove_dir_all(&paths.upgrade_backup_dir);
            }
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&stage_dir);
        unpack_res.context("自动解包官方 Antigravity IDE.exe 安装包失败")?;

        report.unpacked_installer = true;
        println!("[√] 官方程序内核解包完成，零注册表写入！");
    }

    // 3. 写入基础便携配置文件（仅在缺失时创建，绝不覆盖已有配置）
    if seed_default_portable_configs(paths, cfg)? {
        report.seeded_configs = true;
    }

    // 记录当前便携根路径
    let _ = std::fs::write(
        &paths.root_marker,
        format!("{}\n", paths.root.display()),
    );

    if needs_installer_unpack || interactive_tools_prompt {
        println!("[√] 便携隔离目录与默认配置已自动部署就绪:");
        println!("    - 用户配置与插件 : data/user-data、data/extensions");
        println!("    - 隔离个人档案   : data/profile（含 AppData/Cargo/Rustup/.gemini）");
        println!("    - 缓存统一收口   : data/cache（Temp/Logs/Pip/Npm/UV/Go/Crash）");
        println!("    - 项目工作空间   : projects/");
        println!("    - 原生绿色工具箱 : tools/portable/");
    }

    // 4. 若当前尚无任何工具链（无 Scoop 也无原生便携工具），在交互初始化模式下提示一键部署核心工具
    let has_any_tools = has_installed_toolchain(paths);
    if !has_any_tools && interactive_tools_prompt {
        println!("\n[提示] 检测到 tools/ 下尚无开发运行环境（如 Python/uv、Git、Node.js）。");
        if prompt_confirm("是否立即在线部署轻量零软链接工具包 (uv + 7zip + mingit)？[y/N]: ", false) {
            for tool in ["7zip", "uv", "mingit"] {
                if let Err(err) = install_portable_tool(paths, cfg, tool, None) {
                    println!("  [警告] 安装 {tool} 失败（可稍后用 tools install 重试）: {err}");
                }
            }
        }
    }

    Ok(report)
}

/// 生成缺失的默认便携配置文件（data/argv.json、settings.json）
pub fn seed_default_portable_configs(paths: &PortablePaths, cfg: &LauncherConfig) -> Result<bool> {
    let mut created_any = false;

    if !paths.argv_json.exists() {
        let locale = if cfg.launch.locale.trim().is_empty() {
            "zh-cn"
        } else {
            cfg.launch.locale.trim()
        };
        let argv = serde_json::json!({
            "enable-crash-reporter": false,
            "locale": locale
        });
        std::fs::write(&paths.argv_json, serde_json::to_string_pretty(&argv)?)?;
        created_any = true;
    }

    if !paths.settings_json.exists() {
        if let Some(parent) = paths.settings_json.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&paths.settings_json, "{}\n")?;
        created_any = true;
    }

    Ok(created_any)
}

fn has_installed_toolchain(paths: &PortablePaths) -> bool {
    if paths.scoop_dir.join("apps").is_dir() {
        return true;
    }
    if let Ok(entries) = std::fs::read_dir(&paths.portable_tools_dir) {
        if entries.flatten().any(|e| e.path().is_dir()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploys_full_portable_environment_when_only_two_exes_present() {
        let tmp = std::env::temp_dir().join(format!("aiderun_boot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("resources")).unwrap();
        std::fs::write(tmp.join("AideRun.exe"), b"launcher").unwrap();
        std::fs::write(tmp.join("Antigravity IDE.exe"), b"real_ide_stub").unwrap();

        let paths = PortablePaths::from_root(tmp.clone(), tmp.join("AideRun.exe"));
        let cfg = LauncherConfig::default();

        let report = ensure_portable_ready(&paths, &cfg, false).unwrap();
        assert!(report.deployed_skeleton);
        assert!(report.seeded_configs);
        assert!(!report.unpacked_installer);
        assert!(paths.argv_json.is_file());
        assert!(paths.settings_json.is_file());
        assert!(paths.root_marker.is_file());
        assert!(paths.projects_dir.is_dir());
        assert!(paths.portable_tools_dir.is_dir());

        let argv_text = std::fs::read_to_string(&paths.argv_json).unwrap();
        assert!(argv_text.contains("zh-cn"));

        // 二次检测应直接跳过部署，保持幂等
        let report2 = ensure_portable_ready(&paths, &cfg, false).unwrap();
        assert!(!report2.deployed_skeleton);
        assert!(!report2.seeded_configs);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn inspects_missing_vs_ready_portable_environment() {
        let tmp = std::env::temp_dir().join(format!("aiderun_env_status_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("AideRun.exe"), b"launcher").unwrap();

        let paths = PortablePaths::from_root(tmp.clone(), tmp.join("AideRun.exe"));
        assert_eq!(
            inspect_portable_env_status(&paths),
            PortableEnvStatus::MissingIdeEnvironment
        );

        std::fs::create_dir_all(tmp.join("resources")).unwrap();
        assert_eq!(
            inspect_portable_env_status(&paths),
            PortableEnvStatus::MissingIdeEnvironment
        );

        std::fs::write(tmp.join("Antigravity IDE.exe"), b"real_ide_stub").unwrap();
        assert_eq!(
            inspect_portable_env_status(&paths),
            PortableEnvStatus::ReadyPortable
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

