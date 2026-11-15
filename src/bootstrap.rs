// AideRun - 首次纯净目录自动便携环境部署、环境完整性诊断与官方安装包原地解包引擎

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::LauncherConfig;
use crate::console::{init_console, prompt_confirm, ConsoleModeKind};
use crate::env::apply_proxy_and_tools_to_self;
use crate::net::{download_file_with_progress, extract_archive_clean};
use crate::paths::PortablePaths;
use crate::proxy::{parse_proxy_string, read_windows_system_proxy, resolve_and_sync_proxy};
use crate::toolbox::install_portable_tool;
use crate::upgrade::{
    apply_atomic_core_swap, check_latest_release, copy_dir_recursive, is_inno_setup_installer,
    read_local_versions, unpack_ide_package,
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

/// 首次官网便携部署的可选集成组件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeployOptions {
    pub deploy_proxy: bool,
    pub deploy_chinese: bool,
    pub deploy_agent_dev_kit: bool,
}

impl Default for DeployOptions {
    fn default() -> Self {
        Self {
            deploy_proxy: true,
            deploy_chinese: true,
            deploy_agent_dev_kit: true,
        }
    }
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
    options: DeployOptions,
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
            percent: 82,
            stage: "部署核心文件至当前目录".to_string(),
            detail: format!("目标目录: {}", paths.root.display()),
            log_line: Some("[*] 正在将 Antigravity IDE.exe 与 resources/ 写入当前目录...".to_string()),
        });

        apply_atomic_core_swap(&payload_dir, paths).context("写入便携版核心程序文件失败")?;
        if paths.upgrade_backup_dir.is_dir() {
            let _ = std::fs::remove_dir_all(&paths.upgrade_backup_dir);
        }

        // 可选部署 1：免 TUN 强制代理工具 (antigravity-proxy v2.6.3)
        if options.deploy_proxy {
            on_progress(DeployProgress {
                percent: 86,
                stage: "部署免 TUN 强制代理工具 (v2.6.3)".to_string(),
                detail: "正在从开源仓库下载并安装 version.dll 与 config.json...".to_string(),
                log_line: Some("[*] 正在获取免 TUN 强制代理工具 (antigravity-proxy v2.6.3)...".to_string()),
            });
            match deploy_antigravity_proxy(paths, cfg, &stage_dir) {
                Ok(()) => {
                    on_progress(DeployProgress {
                        percent: 90,
                        stage: "免 TUN 代理工具部署完成".to_string(),
                        detail: "已释放 version.dll 与 config.json，并同步本地代理配置".to_string(),
                        log_line: Some("[√] 免 TUN 强制代理工具部署成功！无需开启 TUN 模式即可直连".to_string()),
                    });
                }
                Err(err) => {
                    on_progress(DeployProgress {
                        percent: 90,
                        stage: "代理工具部署提示".to_string(),
                        detail: format!("代理组件下载跳过: {err}"),
                        log_line: Some(format!("[!] 下载免 TUN 代理工具遇到网络警告（已跳过）: {err}")),
                    });
                }
            }
        }

        // 可选部署 2：官方简体中文语言包 (ms-ceintl.vscode-language-pack-zh-hans)
        if options.deploy_chinese {
            on_progress(DeployProgress {
                percent: 91,
                stage: "部署官方简体中文语言包".to_string(),
                detail: "正在下载并配置 ms-ceintl.vscode-language-pack-zh-hans...".to_string(),
                log_line: Some("[*] 正在从官方扩展源下载简体中文语言包...".to_string()),
            });
            match deploy_chinese_language_pack(paths, &stage_dir) {
                Ok(()) => {
                    on_progress(DeployProgress {
                        percent: 95,
                        stage: "官方简体中文语言包部署完成".to_string(),
                        detail: "已就绪：原生全汉化、已勾选自动更新、locale 设置为 zh-cn".to_string(),
                        log_line: Some("[√] 官方简体中文语言包集成完成，已开启扩展自动更新".to_string()),
                    });
                }
                Err(err) => {
                    on_progress(DeployProgress {
                        percent: 95,
                        stage: "中文包部署提示".to_string(),
                        detail: format!("中文包下载跳过: {err}"),
                        log_line: Some(format!("[!] 下载官方中文语言包遇到网络警告（已跳过）: {err}")),
                    });
                }
            }
        }

        // 可选部署 3：AI 编程前沿增强套件 (Modern Web Guidance + Context7 + Chrome DevTools)
        if options.deploy_agent_dev_kit {
            on_progress(DeployProgress {
                percent: 96,
                stage: "部署 AI 编程前沿增强套件".to_string(),
                detail: "正在部署 Google 现代 Web 规范、Context7 及 DevTools 知识库...".to_string(),
                log_line: Some("[*] 正在部署 AI 编程前沿增强套件 (Modern Web + Context7 + DevTools)...".to_string()),
            });
            match deploy_agent_dev_kit(paths) {
                Ok(()) => {
                    on_progress(DeployProgress {
                        percent: 98,
                        stage: "AI 编程增强套件部署完成".to_string(),
                        detail: "已就绪：前沿 Web 规范真理库、实时官方文档与调试工具已全部激活".to_string(),
                        log_line: Some("[√] AI 编程前沿增强套件部署成功！三合一智能体知识库已激活".to_string()),
                    });
                }
                Err(err) => {
                    on_progress(DeployProgress {
                        percent: 98,
                        stage: "AI 增强套件提示".to_string(),
                        detail: format!("增强套件部署跳过: {err}"),
                        log_line: Some(format!("[!] 部署 AI 增强套件遇到警告（已跳过）: {err}")),
                    });
                }
            }
        }

        on_progress(DeployProgress {
            percent: 99,
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

/// 下载并部署 antigravity-proxy v2.6.3 免 TUN 代理工具，并根据当前网络智能同步 proxy 参数
pub fn deploy_antigravity_proxy(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    stage_dir: &Path,
) -> Result<()> {
    let zip_path = stage_dir.join("antigravity-proxy.zip");
    let extract_dir = stage_dir.join("proxy_extract");
    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(&zip_path);

    let urls = [
        "https://github.com/yuaotian/antigravity-proxy/releases/download/v2.6.3/antigravity-proxy-v2.6.3-ide-win-x64.zip",
        "https://ghproxy.net/https://github.com/yuaotian/antigravity-proxy/releases/download/v2.6.3/antigravity-proxy-v2.6.3-ide-win-x64.zip",
    ];

    let mut downloaded = false;
    for url in urls {
        if download_file_with_progress(url, &zip_path, |_, _, _| {}).is_ok() {
            downloaded = true;
            break;
        }
    }
    if !downloaded {
        anyhow::bail!("无法从开源仓库下载免 TUN 代理工具 release 资源");
    }

    extract_archive_clean(&zip_path, &extract_dir, paths, false)?;

    // 递归查找 version.dll 与 config.json
    let mut found_dll: Option<PathBuf> = None;
    let mut found_cfg: Option<PathBuf> = None;
    for entry in walkdir::WalkDir::new(&extract_dir).into_iter().flatten() {
        let path = entry.path();
        if path.is_file() {
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if fname.eq_ignore_ascii_case("version.dll") && found_dll.is_none() {
                found_dll = Some(path.to_path_buf());
            } else if fname.eq_ignore_ascii_case("config.json") && found_cfg.is_none() {
                found_cfg = Some(path.to_path_buf());
            }
        }
    }

    let dll_src = found_dll.context("未在代理工具压缩包中找到 version.dll")?;
    let cfg_src = found_cfg.context("未在代理工具压缩包中找到 config.json")?;

    let dest_dll = paths.root.join("version.dll");
    let dest_cfg = paths.root.join("config.json");

    std::fs::copy(&dll_src, &dest_dll)?;

    // 读取 config.json 并根据当前网络环境智能更新 proxy 端口
    let mut cfg_val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg_src)?)
        .unwrap_or_else(|_| serde_json::json!({}));

    // 智能获取当前代理端点
    let active_ep = cfg
        .proxy
        .manual_proxy
        .as_deref()
        .and_then(parse_proxy_string)
        .or_else(read_windows_system_proxy);

    if let Some(ep) = active_ep {
        if let Some(obj) = cfg_val.as_object_mut() {
            obj.insert(
                "proxy".to_string(),
                serde_json::json!({
                    "type": ep.proxy_type,
                    "host": ep.host,
                    "port": ep.port
                }),
            );
        }
    }

    std::fs::write(&dest_cfg, serde_json::to_string_pretty(&cfg_val)?)?;
    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(&zip_path);
    Ok(())
}

/// 从官方扩展源下载并部署微软官方简体中文语言包，并在 extensions.json 与 settings.json 中开启自动更新
pub fn deploy_chinese_language_pack(paths: &PortablePaths, stage_dir: &Path) -> Result<()> {
    let zip_path = stage_dir.join("chinese_lang_pack.zip");
    let extract_dir = stage_dir.join("lang_extract");
    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(&zip_path);

    let urls = [
        "https://marketplace.visualstudio.com/_apis/public/gallery/publishers/MS-CEINTL/vsextensions/vscode-language-pack-zh-hans/latest/vspackage",
        "https://open-vsx.org/api/MS-CEINTL/vscode-language-pack-zh-hans/latest/file/MS-CEINTL.vscode-language-pack-zh-hans-latest.vsix",
    ];

    let mut downloaded = false;
    for url in urls {
        if download_file_with_progress(url, &zip_path, |_, _, _| {}).is_ok() {
            downloaded = true;
            break;
        }
    }
    if !downloaded {
        anyhow::bail!("无法从官方扩展源下载简体中文语言包");
    }

    extract_archive_clean(&zip_path, &extract_dir, paths, false)?;

    // 查找 extension/package.json 获取真实版本号
    let pkg_json_path = extract_dir.join("extension").join("package.json");
    let mut ver_str = "1.106.0".to_string();
    if pkg_json_path.is_file() {
        if let Ok(txt) = std::fs::read_to_string(&pkg_json_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                if let Some(v_str) = v.get("version").and_then(|s| s.as_str()) {
                    ver_str = v_str.to_string();
                }
            }
        }
    }

    let folder_name = format!("ms-ceintl.vscode-language-pack-zh-hans-{ver_str}-universal");
    let dest_ext_dir = paths.extensions_dir.join(&folder_name);
    let _ = std::fs::remove_dir_all(&dest_ext_dir);
    std::fs::create_dir_all(&dest_ext_dir)?;

    let src_ext_dir = extract_dir.join("extension");
    if src_ext_dir.is_dir() {
        copy_dir_recursive(&src_ext_dir, &dest_ext_dir)?;
    }
    let manifest_src = extract_dir.join("extension.vsixmanifest");
    if manifest_src.is_file() {
        let _ = std::fs::copy(&manifest_src, dest_ext_dir.join(".vsixmanifest"));
    }

    // 更新 data/extensions/extensions.json
    let ext_meta_file = paths.extensions_dir.join("extensions.json");
    let mut list: Vec<serde_json::Value> = if ext_meta_file.is_file() {
        std::fs::read_to_string(&ext_meta_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    list.retain(|item| {
        item.get("identifier")
            .and_then(|id| id.get("id"))
            .and_then(|s| s.as_str())
            .map(|s| !s.eq_ignore_ascii_case("ms-ceintl.vscode-language-pack-zh-hans"))
            .unwrap_or(true)
    });

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1790124951582);

    let fs_path = dest_ext_dir.display().to_string();
    let url_slash = fs_path.replace('\\', "/");
    let external_uri = if url_slash.starts_with('/') {
        format!("file://{url_slash}")
    } else {
        format!("file:///{url_slash}")
    };
    let uri_path = if url_slash.starts_with('/') {
        url_slash
    } else {
        format!("/{url_slash}")
    };

    let new_entry = serde_json::json!({
        "identifier": {
            "id": "ms-ceintl.vscode-language-pack-zh-hans"
        },
        "version": ver_str,
        "location": {
            "$mid": 1,
            "fsPath": fs_path,
            "_sep": 1,
            "external": external_uri,
            "path": uri_path,
            "scheme": "file"
        },
        "relativeLocation": folder_name,
        "metadata": {
            "isApplicationScoped": true,
            "installedTimestamp": now_ts,
            "pinned": false,
            "source": "gallery",
            "id": "e4ee7751-6514-4731-9cdb-7580ffa9e70b",
            "publisherId": "0b0882c3-aee3-4d7c-b5f9-872f9be0a115",
            "publisherDisplayName": "MS-CEINTL",
            "targetPlatform": "universal",
            "updated": false,
            "private": false,
            "isPreReleaseVersion": false,
            "hasPreReleaseVersion": false
        }
    });

    list.push(new_entry);
    std::fs::write(&ext_meta_file, serde_json::to_string_pretty(&list)?)?;

    // 确保 argv.json 设置 locale = zh-cn
    let mut argv_val: serde_json::Value = if paths.argv_json.is_file() {
        std::fs::read_to_string(&paths.argv_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if let Some(obj) = argv_val.as_object_mut() {
        obj.insert("locale".to_string(), serde_json::json!("zh-cn"));
        obj.entry("enable-crash-reporter".to_string())
            .or_insert_with(|| serde_json::json!(false));
    }
    std::fs::write(&paths.argv_json, serde_json::to_string_pretty(&argv_val)?)?;

    // 确保 settings.json 开启 extensions.autoUpdate = true
    if let Some(p) = paths.settings_json.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let mut settings_val: serde_json::Value = if paths.settings_json.is_file() {
        std::fs::read_to_string(&paths.settings_json)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if let Some(obj) = settings_val.as_object_mut() {
        obj.insert("extensions.autoUpdate".to_string(), serde_json::json!(true));
    }
    std::fs::write(&paths.settings_json, serde_json::to_string_pretty(&settings_val)?)?;

    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(&zip_path);
    Ok(())
}

/// 部署 AI 编程前沿增强套件：释放 Modern Web Guidance 核心技能知识库，并在 .gemini/config/config.json 中激活三大高价值插件 (Modern Web + Context7 + DevTools)
pub fn deploy_agent_dev_kit(paths: &PortablePaths) -> Result<()> {
    let gemini_config_dir = paths.gemini_dir.join("config");
    let plugins_dir = gemini_config_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir)?;

    let mw_dir = plugins_dir.join("modern-web-guidance-plugin");
    let mw_skills_dir = mw_dir.join("skills").join("modern-web-guidance");
    std::fs::create_dir_all(&mw_skills_dir)?;

    // 1. 写入 Modern Web Guidance 的 plugin.json
    let plugin_json_path = mw_dir.join("plugin.json");
    if !plugin_json_path.exists() {
        let plugin_json_content = r#"{
  "name": "modern-web-guidance-plugin",
  "displayName": "Modern Web Guidance",
  "description": "Keep your coding agent up to date with the latest web best practices.",
  "logo": "assets/logo.svg",
  "author": {
    "name": "Google Chrome",
    "url": "https://developer.chrome.com"
  },
  "homepage": "https://developer.chrome.com/docs/modern-web-guidance",
  "repository": "https://github.com/GoogleChrome/modern-web-guidance",
  "license": "Apache-2.0",
  "version": "1.0.0"
}
"#;
        let _ = std::fs::write(&plugin_json_path, plugin_json_content);
    }

    // 2. 写入 Modern Web Guidance 的 gemini-extension.json
    let gemini_ext_path = mw_dir.join("gemini-extension.json");
    if !gemini_ext_path.exists() {
        let gemini_ext_content = r#"{
  "name": "modern-web-guidance-plugin",
  "skills": [
    "skills/modern-web-guidance"
  ]
}
"#;
        let _ = std::fs::write(&gemini_ext_path, gemini_ext_content);
    }

    // 3. 写入 Modern Web Guidance 核心 SKILL.md
    let skill_md_path = mw_skills_dir.join("SKILL.md");
    if !skill_md_path.exists() {
        let skill_md_content = include_str!("../assets/modern_web_skill.md");
        let _ = std::fs::write(&skill_md_path, skill_md_content);
    }

    // 4. 原子化合并激活 config.json (将 modern-web-guidance-plugin, context7, chrome-devtools-plugin 注册并设为 enabled)
    let config_json_path = gemini_config_dir.join("config.json");
    let mut config_val: serde_json::Value = if config_json_path.is_file() {
        match std::fs::read_to_string(&config_json_path) {
            Ok(txt) => serde_json::from_str(&txt).unwrap_or_else(|_| serde_json::json!({})),
            Err(_) => serde_json::json!({}),
        }
    } else {
        serde_json::json!({})
    };

    if !config_val.is_object() {
        config_val = serde_json::json!({});
    }

    let plugins_obj = config_val
        .as_object_mut()
        .unwrap()
        .entry("plugins")
        .or_insert_with(|| serde_json::json!({}));

    if let Some(plugins_map) = plugins_obj.as_object_mut() {
        // (1) 注册 Modern Web Guidance 插件
        plugins_map.insert(
            "modern-web-guidance-plugin".to_string(),
            serde_json::json!({
                "enabled": true,
                "installedFrom": {
                    "id": "antigravity-plugins-official/plugin/modern-web-guidance-plugin",
                    "marketplace": "antigravity-plugins-official",
                    "version": "1.0.0"
                }
            }),
        );

        // (2) 注册 Context7 实时最新库官方文档插件
        plugins_map.entry("context7".to_string()).or_insert_with(|| {
            serde_json::json!({
                "enabled": true,
                "installedFrom": {
                    "id": "antigravity-plugins-partners/plugin/context7",
                    "marketplace": "antigravity-plugins-partners"
                }
            })
        });

        // (3) 注册 Chrome DevTools 自动化与排错插件
        plugins_map.entry("chrome-devtools-plugin".to_string()).or_insert_with(|| {
            serde_json::json!({
                "enabled": true,
                "installedFrom": {
                    "id": "antigravity-plugins-official/plugin/chrome-devtools-plugin",
                    "marketplace": "antigravity-plugins-official"
                }
            })
        });
    }

    let formatted_json = serde_json::to_string_pretty(&config_val)?;
    std::fs::write(&config_json_path, formatted_json)?;

    Ok(())
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

    #[test]
    fn deploy_options_defaults_to_true() {
        let opts = DeployOptions::default();
        assert!(opts.deploy_proxy);
        assert!(opts.deploy_chinese);
        assert!(opts.deploy_agent_dev_kit);
    }
}

