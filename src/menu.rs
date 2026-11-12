// AideRun - 交互式多功能控制台菜单模块（整合启动、工具箱、清理、脱敏、升级与体检）

use anyhow::Result;

use crate::bootstrap::ensure_portable_ready;
use crate::clean::{run_clean, CleanLevel};
use crate::config::LauncherConfig;
use crate::console::{pause_enter, prompt_line};
use crate::doctor::run_doctor;
use crate::launch::{create_desktop_shortcut, launch_ide, open_portable_shell};
use crate::paths::PortablePaths;
use crate::proxy::resolve_and_sync_proxy;
use crate::sanitize::run_sanitize;
use crate::toolbox::{install_portable_tool, list_tools};
use crate::upgrade::{handle_check_update, handle_upgrade_command};

/// 运行交互式菜单循环
pub fn run_interactive_menu(paths: &PortablePaths, cfg: &mut LauncherConfig) -> Result<()> {
    loop {
        println!("\n==================================================================");
        println!("               AideRun - Antigravity IDE 便携控制中心");
        println!("==================================================================");
        println!("  当前便携目录 : {}", paths.root.display());
        println!();
        println!("  [1] 启动 Antigravity IDE（完整隔离环境）");
        println!("  [2] 打开便携开发终端（CMD / PowerShell 已注入全套环境变量）");
        println!("  [3] 智能垃圾清理（先统计各目录体积并按等级清理）");
        println!("  [4] 官方新版检测与无痕升级（antigravity.google 检测 / 本地安装包升级）");
        println!("  [5] 打包前隐私脱敏与极限瘦身（保留简体中文与免权限规则）");
        println!("  [6] 便携工具箱管理（查看/安装 uv、MinGit、Node、7zip 或修复 Scoop）");
        println!("  [7] 一键便携环境健康体检 (doctor)");
        println!("  [8] 主动部署便携骨架 / 初始化 launcher.toml / 创建桌面快捷方式");
        println!("  [0] 退出");
        println!();

        let choice = prompt_line("请输入选项编号 [0-8]: ")?;
        match choice.as_str() {
            "0" | "q" | "exit" => break,
            "1" => {
                if let Err(err) = launch_ide(paths, cfg, &[]) {
                    println!("[错误] 启动失败: {err}");
                    pause_enter();
                } else {
                    println!("[√] Antigravity IDE 已启动。");
                    break;
                }
            }
            "2" => {
                let ps = prompt_line("选择终端类型 [1] CMD (默认)  [2] PowerShell: ")? == "2";
                if let Err(err) = open_portable_shell(paths, cfg, ps) {
                    println!("[错误] {err}");
                    pause_enter();
                }
            }
            "3" => {
                handle_clean_submenu(paths, cfg);
                pause_enter();
            }
            "4" => {
                handle_upgrade_submenu(paths, cfg);
                pause_enter();
            }
            "5" => {
                let full = prompt_line("是否连带执行彻底缓存瘦身以便直接打包？[Y/n]: ")?
                    .to_ascii_lowercase()
                    != "n";
                if let Err(err) = run_sanitize(paths, &cfg.cleanup, full, false) {
                    println!("[错误] {err}");
                }
                pause_enter();
            }
            "6" => {
                handle_tools_submenu(paths, cfg);
                pause_enter();
            }
            "7" => {
                if let Err(err) = run_doctor(paths, cfg) {
                    println!("[错误] {err}");
                }
                pause_enter();
            }
            "8" => {
                handle_setup_submenu(paths, cfg);
                pause_enter();
            }
            _ => println!("无效选项，请输入 0-8。"),
        }
    }
    Ok(())
}

fn handle_clean_submenu(paths: &PortablePaths, cfg: &LauncherConfig) {
    println!("\n--- 智能清理模式 ---");
    println!("  [1] 仅预览各目录可清理空间 (Dry-Run)");
    println!("  [2] 轻量清理 (Temp、Logs、Crash Dumps、Proxy Log)");
    println!("  [3] 深度清理 (含 Chromium Web/Code/GPU/Extension 缓存)");
    println!("  [4] 彻底瘦身 (含 pip/npm/uv/Cargo/Scoop 安装包缓存)");
    let sel = prompt_line("请选择 [1-4]: ").unwrap_or_default();
    let res = match sel.as_str() {
        "1" => run_clean(paths, &cfg.cleanup, CleanLevel::All, true, true),
        "2" => run_clean(paths, &cfg.cleanup, CleanLevel::Light, false, false),
        "3" => run_clean(paths, &cfg.cleanup, CleanLevel::Deep, false, false),
        "4" => run_clean(paths, &cfg.cleanup, CleanLevel::All, false, false),
        _ => return,
    };
    if let Err(err) = res {
        println!("[错误] {err}");
    }
}

fn handle_upgrade_submenu(paths: &PortablePaths, cfg: &LauncherConfig) {
    println!("\n--- 版本检测与无痕升级 ---");
    println!("  [1] 联网检测 https://antigravity.google 最新版本");
    println!("  [2] 在线自动下载并无痕升级（零注册表）");
    println!("  [3] 从本地已下载的单文件安装包 (如 Antigravity IDE.exe) 升级");
    println!("  [4] 回滚至升级前旧版本快照 (--rollback)");
    println!("  [5] 清理历史升级备份快照 (--clean-backup)");
    let sel = prompt_line("请选择 [1-5]: ").unwrap_or_default();
    let res = match sel.as_str() {
        "1" => handle_check_update(paths, cfg),
        "2" => handle_upgrade_command(&[], paths, cfg),
        "3" => {
            let path_input = prompt_line("请拖入或粘贴安装包完整路径: ").unwrap_or_default();
            let trimmed = path_input.trim().trim_matches('"').to_string();
            if trimmed.is_empty() {
                return;
            }
            handle_upgrade_command(&[trimmed], paths, cfg)
        }
        "4" => handle_upgrade_command(&["--rollback".to_string()], paths, cfg),
        "5" => handle_upgrade_command(&["--clean-backup".to_string()], paths, cfg),
        _ => return,
    };
    if let Err(err) = res {
        println!("[错误] {err}");
    }
}

fn handle_tools_submenu(paths: &PortablePaths, cfg: &mut LauncherConfig) {
    println!("\n--- 便携工具箱管理 ---");
    println!("  [1] 查看已安装组件与运行时版本探测");
    println!("  [2] 一键安装原生零软链接组件 (uv / 7zip / innoextract / mingit / nodejs)");
    println!("  [3] 修复现有 Scoop 跨盘符软链接与 Shim 路径");
    let sel = prompt_line("请选择 [1-3]: ").unwrap_or_default();
    let proxy = resolve_and_sync_proxy(&paths.proxy_config, &cfg.proxy);
    match sel.as_str() {
        "1" => {
            if let Err(err) = list_tools(paths, cfg, proxy.as_ref()) {
                println!("[错误] {err}");
            }
        }
        "2" => {
            let name = prompt_line("请输入要安装的组件名称 (uv/7zip/innoextract/mingit/nodejs): ")
                .unwrap_or_default();
            if !name.trim().is_empty() {
                if let Err(err) = install_portable_tool(paths, cfg, name.trim(), None) {
                    println!("[错误] {err}");
                }
            }
        }
        "3" => {
            if let Err(err) = crate::toolbox::handle_tools_command(
                &["heal".to_string()],
                paths,
                cfg,
                proxy.as_ref(),
            ) {
                println!("[错误] {err}");
            }
        }
        _ => {}
    }
}

fn handle_setup_submenu(paths: &PortablePaths, cfg: &LauncherConfig) {
    println!("\n--- 初始化与辅助设置 ---");
    println!("  [1] 自动补全/部署全套便携目录结构与基础配置");
    println!("  [2] 生成自定义配置文件模板 (data/launcher.toml)");
    println!("  [3] 在桌面创建 Antigravity便携版.lnk 快捷方式");
    let sel = prompt_line("请选择 [1-3]: ").unwrap_or_default();
    match sel.as_str() {
        "1" => {
            if let Err(err) = ensure_portable_ready(paths, cfg, true) {
                println!("[错误] {err}");
            }
        }
        "2" => {
            if let Err(err) = cfg.save(&paths.launcher_config) {
                println!("[错误] {err}");
            } else {
                println!("[√] 已保存配置模板: {}", paths.rel_display(&paths.launcher_config));
            }
        }
        "3" => {
            if let Err(err) = create_desktop_shortcut(paths) {
                println!("[错误] {err}");
            } else {
                println!("[√] 已在桌面创建快捷方式。");
            }
        }
        _ => {}
    }
}

