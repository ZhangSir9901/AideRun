// AideRun - 程序主入口，负责玻璃拟态面板默认展示、静默分流启动、首次部署与子命令路由
#![windows_subsystem = "windows"]

mod bootstrap;
mod clean;
mod config;
mod console;
mod doctor;
mod env;
mod gui;
mod launch;
mod menu;
mod net;
mod paths;
mod preboot_gui;
mod process;
mod proxy;
mod sanitize;
mod scoop_heal;
mod toolbox;
mod upgrade;

use std::io::Write;

use anyhow::Result;
use chrono::Local;

use crate::bootstrap::ensure_portable_ready;
use crate::clean::{run_clean, CleanLevel};
use crate::config::LauncherConfig;
use crate::console::{init_console, pause_enter, show_error_dialog, show_info_dialog, ConsoleModeKind};
use crate::doctor::run_doctor;
use crate::env::apply_proxy_and_tools_to_self;
use crate::gui::run_panel;
use crate::launch::{
    create_desktop_shortcut, launch_ide, launch_ide_isolated_instance, open_portable_shell,
};
use crate::menu::run_interactive_menu;
use crate::paths::PortablePaths;
use crate::proxy::resolve_and_sync_proxy;
use crate::sanitize::run_sanitize;
use crate::toolbox::{handle_tools_command, install_portable_tool};
use crate::upgrade::{handle_check_update, handle_upgrade_command};

const RESERVED_SUBCOMMANDS: &[&str] = &[
    "menu",
    "shell",
    "cmd",
    "powershell",
    "ps",
    "tools",
    "clean",
    "sanitize",
    "pack-prep",
    "check-update",
    "upgrade",
    "doctor",
    "init",
    "bootstrap",
    "shortcut",
    "config",
    "help",
    "--help",
    "-h",
    "--version",
    "-v",
    "-V",
];

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "未知运行时异常".to_string()
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
        let report = format!("[{ts}] PANIC @ {loc}\n原因: {msg}\n");

        if let Ok(paths) = PortablePaths::discover() {
            let log_dir = paths.logs_dir;
            if std::fs::create_dir_all(&log_dir).is_ok() {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_dir.join("panic.log"))
                {
                    let _ = writeln!(f, "{report}---");
                }
            }
        }
        eprintln!("[致命异常] {report}");
        show_error_dialog(
            "AideRun 运行异常保护",
            &format!(
                "检测到未捕获异常，已阻止静默闪退并写入 data/cache/logs/panic.log：\n\n{report}"
            ),
        );
    }));
}

fn main() {
    install_panic_hook();
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    // 1. 默认双击无参数，或显式传入 `panel` / `gui` 时，弹出 860x560 水晶玻璃拟态控制面板（选项 A）
    if raw_args.is_empty() || matches!(raw_args[0].as_str(), "panel" | "gui") {
        let attached = init_console(
            ConsoleModeKind::AttachIfPresent,
            "AideRun - Antigravity IDE 便携控制台",
        );
        if let Err(err) = run_panel() {
            eprintln!("[错误] AideRun 面板启动失败: {err:#}");
            if !attached {
                show_error_dialog("AideRun 面板启动失败", &format!("{err:#}"));
            }
            std::process::exit(1);
        }
        return;
    }

    // 2. 支持 `--silent` / `-s`、`--instance <name>` 或直接拖放项目文件夹静默直启 IDE
    let is_cli_subcommand = RESERVED_SUBCOMMANDS.contains(&raw_args[0].as_str());
    if !is_cli_subcommand {
        if let Err(err) = run_silent_launch(&raw_args) {
            show_error_dialog("AideRun 启动失败", &format!("{err:#}"));
            std::process::exit(1);
        }
        return;
    }

    let first = raw_args[0].as_str();
    let interactive_subcommand = matches!(first, "menu" | "shell" | "cmd" | "powershell" | "ps" | "init" | "bootstrap");
    let allocated_window = init_console(
        if interactive_subcommand {
            ConsoleModeKind::EnsureInteractive
        } else {
            ConsoleModeKind::AttachIfPresent
        },
        "AideRun - Antigravity IDE 便携控制中心",
    );

    if let Err(err) = run_cli_subcommand(&raw_args) {
        eprintln!("[错误] {err:#}");
        if allocated_window {
            pause_enter();
        }
        std::process::exit(1);
    }
}

fn run_silent_launch(raw_args: &[String]) -> Result<()> {
    let paths = PortablePaths::discover()?;
    let cfg = LauncherConfig::load_or_default(&paths.launcher_config);

    // 解析可选 `--silent` / `-s` 与 `--instance <名称>`
    let mut instance_name: Option<String> = None;
    let mut pass_args = Vec::<String>::new();
    let mut idx = 0usize;
    while idx < raw_args.len() {
        let cur = &raw_args[idx];
        if cur == "--silent" || cur == "-s" {
            idx += 1;
        } else if cur == "--instance" {
            if let Some(val) = raw_args.get(idx + 1) {
                instance_name = Some(val.clone());
                idx += 2;
            } else {
                idx += 1;
            }
        } else {
            pass_args.push(cur.clone());
            idx += 1;
        }
    }

    if let Some(inst) = instance_name {
        let target = pass_args.first().map(std::path::Path::new);
        let _ = launch_ide_isolated_instance(&paths, &cfg, &inst, true, target)?;
        Ok(())
    } else {
        launch_ide(&paths, &cfg, &pass_args)
    }
}

fn run_cli_subcommand(args: &[String]) -> Result<()> {
    let paths = PortablePaths::discover()?;
    let mut cfg = LauncherConfig::load_or_default(&paths.launcher_config);

    // 全局提前注入代理与本地工具路径，保证 upgrade / check-update / tools 命令直接走便携代理
    let proxy = resolve_and_sync_proxy(&paths.proxy_config, &cfg.proxy);
    apply_proxy_and_tools_to_self(&paths, &cfg, proxy.as_ref());

    let sub = args[0].as_str();
    let rest = &args[1..];

    match sub {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "--version" | "-v" | "-V" => {
            println!("AideRun {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "menu" => run_interactive_menu(&paths, &mut cfg),
        "shell" | "cmd" => open_portable_shell(&paths, &cfg, false),
        "powershell" | "ps" => open_portable_shell(&paths, &cfg, true),
        "init" | "bootstrap" => {
            let with_tools = rest.iter().any(|a| a == "--with-tools");
            ensure_portable_ready(&paths, &cfg, !with_tools)?;
            if with_tools {
                for tool in ["7zip", "uv", "mingit"] {
                    install_portable_tool(&paths, &cfg, tool, None)?;
                }
            }
            Ok(())
        }
        "tools" => handle_tools_command(rest, &paths, &mut cfg, proxy.as_ref()),
        "clean" => {
            let dry_run = rest.iter().any(|a| a == "--dry-run" || a == "-n");
            let auto_yes = rest.iter().any(|a| a == "--yes" || a == "-y");
            let level = if rest.iter().any(|a| a == "--all") {
                CleanLevel::All
            } else if rest.iter().any(|a| a == "--light") {
                CleanLevel::Light
            } else {
                CleanLevel::Deep
            };
            let _ = run_clean(&paths, &cfg.cleanup, level, dry_run, auto_yes)?;
            Ok(())
        }
        "sanitize" => {
            let auto_yes = rest.iter().any(|a| a == "--yes" || a == "-y");
            let full = rest.iter().any(|a| a == "--full");
            let _ = run_sanitize(&paths, &cfg.cleanup, full, auto_yes)?;
            Ok(())
        }
        "pack-prep" => {
            let auto_yes = rest.iter().any(|a| a == "--yes" || a == "-y");
            let _ = run_sanitize(&paths, &cfg.cleanup, true, auto_yes)?;
            Ok(())
        }
        "check-update" => handle_check_update(&paths, &cfg),
        "upgrade" => handle_upgrade_command(rest, &paths, &cfg),
        "doctor" => run_doctor(&paths, &cfg),
        "shortcut" => {
            create_desktop_shortcut(&paths)?;
            println!("[√] 已在当前用户桌面创建 Antigravity便携版.lnk。");
            if rest.iter().any(|a| a == "--dialog") {
                show_info_dialog("AideRun", "已在桌面创建 Antigravity便携版.lnk 快捷方式。");
            }
            Ok(())
        }
        "config" => {
            if rest.first().map(|s| s.as_str()) == Some("init") {
                cfg.save(&paths.launcher_config)?;
                println!("[√] 已生成配置模板: {}", paths.rel_display(&paths.launcher_config));
            } else {
                println!("配置文件路径: {}", paths.launcher_config.display());
                println!("存在状态    : {}", paths.launcher_config.is_file());
                println!("生成模板命令: AideRun.exe config init");
            }
            Ok(())
        }
        _ => {
            print_help();
            Ok(())
        }
    }
}

fn print_help() {
    println!("AideRun {} - Antigravity IDE 便携版一体化启动器与维护引擎", env!("CARGO_PKG_VERSION"));
    println!();
    println!("默认行为：");
    println!("  无参数双击 (`panel` / `gui`)   打开 860x560 水晶玻璃拟态启动面板（托盘常驻、多开与维护）");
    println!("  `--silent` 或拖放文件夹       跳过面板静默注入便携环境并直接拉起 Antigravity IDE.exe");
    println!("  `--instance <名称> [项目]`      使用独立隔离数据分身 (data/instances/<名称>) 多开窗口");
    println!();
    println!("核心管理命令：");
    println!("  menu                         打开彩色交互式控制中心菜单");
    println!("  init [--with-tools]          主动部署全套便携目录与默认配置（可选同步安装绿色工具链）");
    println!("  shell / powershell           启动注入完整便携环境变量的 CMD / PowerShell 终端");
    println!("  clean [--light|--deep|--all] [--dry-run] [--yes]   智能统计并分级清理垃圾缓存");
    println!("  sanitize [--full] [--yes]    打包前原生 SQLite 隐私脱敏（保留简体中文与免权限规则）");
    println!("  pack-prep [--yes]            一键执行“脱敏 + 彻底瘦身”，准备好直接压缩分发");
    println!("  check-update                 从 antigravity.google/download 与 /releases 检测新版");
    println!("  upgrade [<安装包或URL>] [--check|--rollback|--clean-backup|--yes]");
    println!("                               零注册表解包并原子升级 Antigravity IDE 内核");
    println!("  tools <list|install|upgrade|add-custom|remove>    零软链接原生便携组件管理平台");
    println!("  doctor                       一键便携环境健康体检");
    println!("  shortcut                     按需在桌面创建快捷方式");
    println!("  config [init]                查看或生成 data/launcher.toml 配置模板");
}

