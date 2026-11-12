// AideRun - Antigravity IDE 主程序启动、多开窗口、隔离实例、便携终端与快捷方式模块

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::bootstrap::ensure_portable_ready;
use crate::clean::{run_clean_with_opts, run_smart_daily_clean_if_due, CleanLevel};
use crate::config::LauncherConfig;
use crate::console::CREATE_NO_WINDOW;
use crate::env::{apply_env_to_command, build_isolated_env};
use crate::paths::PortablePaths;
use crate::proxy::resolve_and_sync_proxy;
use crate::relocate::auto_relocate_portable_env;
use crate::scoop_heal::auto_heal_scoop_if_needed;

const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// 静默模式准备环境并拉起 Antigravity IDE.exe
pub fn launch_ide(paths: &PortablePaths, cfg: &LauncherConfig, pass_args: &[String]) -> Result<()> {
    let wait_clean = cfg.launch.auto_clean_on_exit || cfg.launch.smart_clean;
    let effective_args = resolve_default_pass_args(paths, cfg, pass_args);
    spawn_ide_profile(
        paths,
        cfg,
        &paths.user_data_dir,
        &paths.crash_dumps_dir,
        &paths.extensions_dir,
        &effective_args,
        wait_clean,
    )
}

/// 非阻塞启动主环境或新项目窗口（供面板与系统托盘多开调用，支持 `--new-window`）
pub fn launch_ide_new_window(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    target_path: Option<&Path>,
    force_new_window: bool,
) -> Result<()> {
    let mut args = Vec::<String>::new();
    if force_new_window {
        args.push("--new-window".to_string());
    }
    if let Some(target) = target_path {
        args.push(target.display().to_string());
    }
    spawn_ide_profile(
        paths,
        cfg,
        &paths.user_data_dir,
        &paths.crash_dumps_dir,
        &paths.extensions_dir,
        &args,
        false,
    )
}

/// 启动完全隔离双开实例（独立 user-data 与 crash-dumps，位于 `data/instances/<name>/`）
pub fn launch_ide_isolated_instance(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    instance_name: &str,
    share_extensions: bool,
    target_path: Option<&Path>,
) -> Result<String> {
    let safe_name = sanitize_instance_name(instance_name);
    if safe_name.is_empty() {
        anyhow::bail!("实例名称不能为空且仅支持字母、数字、连字符或下划线");
    }

    let inst_root = paths.data_dir.join("instances").join(&safe_name);
    let user_data = inst_root.join("user-data");
    let crash_dumps = inst_root.join("crash-dumps");
    let ext_dir = if share_extensions {
        paths.extensions_dir.clone()
    } else {
        inst_root.join("extensions")
    };
    std::fs::create_dir_all(&user_data)?;
    std::fs::create_dir_all(&crash_dumps)?;
    std::fs::create_dir_all(&ext_dir)?;

    let mut args = vec!["--new-window".to_string()];
    if let Some(target) = target_path {
        args.push(target.display().to_string());
    }

    spawn_ide_profile(paths, cfg, &user_data, &crash_dumps, &ext_dir, &args, false)?;
    Ok(safe_name)
}

fn resolve_default_pass_args(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    pass_args: &[String],
) -> Vec<String> {
    if !pass_args.is_empty() {
        return pass_args.to_vec();
    }
    let ws_name = cfg.launch.default_workspace.trim();
    if !ws_name.is_empty() {
        let ws = paths.root.join(ws_name);
        if ws.exists() {
            return vec![ws.display().to_string()];
        }
    }
    Vec::new()
}

fn spawn_ide_profile(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    user_data_dir: &Path,
    crash_dumps_dir: &Path,
    extensions_dir: &Path,
    pass_args: &[String],
    wait_for_exit: bool,
) -> Result<()> {
    // 1. 检测并自动完成首次便携环境部署
    ensure_portable_ready(paths, cfg, false)?;

    if !paths.ide_exe.is_file() {
        anyhow::bail!(
            "未在当前目录找到 Antigravity IDE.exe！\n目录: {}",
            paths.root.display()
        );
    }

    // 2. 跨盘符无感自愈（检测盘符/路径漂移并自动修复扩展、工作区、Git 凭据与 SQLite 索引）
    let _ = auto_relocate_portable_env(paths, &cfg.toolchain, false);
    if cfg.toolchain.auto_heal_scoop {
        let _ = auto_heal_scoop_if_needed(paths, false);
    }

    // 3. 只读系统代理探测并安全同步至 config.json
    let proxy = resolve_and_sync_proxy(&paths.proxy_config, &cfg.proxy);
    let env_vars = build_isolated_env(paths, cfg, proxy.as_ref());

    // 4. 仅在用户显式启用配置开关时创建桌面快捷方式（默认关闭，不写宿主机桌面）
    if cfg.launch.create_desktop_shortcut {
        let _ = create_desktop_shortcut(paths);
    }

    // 5. 组装 IDE 启动参数
    let mut cmd = Command::new(&paths.ide_exe);
    cmd.current_dir(&paths.root);
    apply_env_to_command(&mut cmd, &env_vars);

    if !cfg.launch.locale.trim().is_empty() {
        cmd.arg("--locale").arg(cfg.launch.locale.trim());
    }
    if cfg.launch.disable_gpu_disk_cache {
        cmd.arg("--disable-gpu-disk-cache");
    }
    cmd.arg("--crash-reporter-directory")
        .arg(crash_dumps_dir)
        .arg("--user-data-dir")
        .arg(user_data_dir)
        .arg("--extensions-dir")
        .arg(extensions_dir);

    for extra in &cfg.launch.extra_args {
        if !extra.trim().is_empty() {
            cmd.arg(extra);
        }
    }
    if !pass_args.is_empty() {
        cmd.args(pass_args);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("启动失败: {}", paths.ide_exe.display()))?;

    // 若开启智能清理或退出后自动清理，则等待主进程结束后静默扫尾（保护面板目录）
    if wait_for_exit {
        let _ = child.wait();
        if cfg.launch.smart_clean {
            let _ = run_smart_daily_clean_if_due(paths, cfg, true);
        } else {
            let _ = run_clean_with_opts(paths, &cfg.cleanup, CleanLevel::Light, false, true, false, true);
        }
    }

    Ok(())
}

/// 扫描 `projects/` 目录下的直属项目子文件夹（按名称排序）
pub fn list_project_folders(paths: &PortablePaths) -> Vec<(String, String)> {
    let mut list = Vec::new();
    let Ok(rd) = std::fs::read_dir(&paths.projects_dir) else {
        return list;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        list.push((name.to_string(), p.display().to_string()));
    }
    list.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    list
}

/// 列出 `data/instances/` 下已创建的独立多开实例名
pub fn list_isolated_instances(paths: &PortablePaths) -> Vec<String> {
    let inst_root = paths.data_dir.join("instances");
    let mut list = Vec::new();
    let Ok(rd) = std::fs::read_dir(&inst_root) else {
        return list;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                list.push(name.to_string());
            }
        }
    }
    list.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    list
}

/// 清洗并校验多开实例名称（防止路径穿越与非法字符）
pub fn sanitize_instance_name(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(32)
        .collect()
}

/// 打开已注入完整便携隔离环境变量的交互式终端（CLI 阻塞模式）
pub fn open_portable_shell(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    use_powershell: bool,
) -> Result<()> {
    let mut cmd = prepare_shell_command(paths, cfg, use_powershell)?;
    println!("[*] 已启动独立便携开发终端（输入 exit 退出返回）。");
    let _ = cmd.status()?;
    Ok(())
}

/// 在独立新控制台窗口启动已注入便携环境的终端（GUI 非阻塞模式）
pub fn spawn_portable_shell_window(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    use_powershell: bool,
) -> Result<()> {
    let mut cmd = prepare_shell_command(paths, cfg, use_powershell)?;
    cmd.creation_flags(CREATE_NEW_CONSOLE);
    cmd.spawn().context("启动便携开发终端窗口失败")?;
    Ok(())
}

fn prepare_shell_command(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    use_powershell: bool,
) -> Result<Command> {
    ensure_portable_ready(paths, cfg, false)?;
    let _ = auto_relocate_portable_env(paths, &cfg.toolchain, false);
    if cfg.toolchain.auto_heal_scoop {
        let _ = auto_heal_scoop_if_needed(paths, false);
    }
    let proxy = resolve_and_sync_proxy(&paths.proxy_config, &cfg.proxy);
    let env_vars = build_isolated_env(paths, cfg, proxy.as_ref());

    let mut cmd = if use_powershell {
        let mut c = Command::new("powershell.exe");
        c.args([
            "-NoExit",
            "-Command",
            "$Host.UI.RawUI.WindowTitle = 'AideRun Portable PowerShell'",
        ]);
        c
    } else {
        let mut c = Command::new("cmd.exe");
        c.args(["/K", "title AideRun Portable Console"]);
        c
    };
    let work_dir = if paths.projects_dir.is_dir() {
        &paths.projects_dir
    } else {
        &paths.root
    };
    cmd.current_dir(work_dir);
    apply_env_to_command(&mut cmd, &env_vars);
    Ok(cmd)
}

/// 弹出 Windows 原生目录选择器（供面板多开选择任意项目目录）
pub fn pick_folder_dialog(title: &str, initial_dir: &Path) -> Option<PathBuf> {
    let init = initial_dir.display().to_string().replace('\'', "''");
    let ttl = title.replace('\'', "''");
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
         Add-Type -AssemblyName System.Windows.Forms; \
         $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
         $d.Description = '{ttl}'; \
         if (Test-Path '{init}') {{ $d.SelectedPath = '{init}' }}; \
         if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{ Write-Output $d.SelectedPath }}"
    );
    run_sta_picker_powershell(&script).map(PathBuf::from)
}

/// 弹出 Windows 原生文件选择器（供面板选择本地 Antigravity IDE 安装包升级）
pub fn pick_installer_file_dialog(initial_dir: &Path) -> Option<PathBuf> {
    let init = initial_dir.display().to_string().replace('\'', "''");
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
         Add-Type -AssemblyName System.Windows.Forms; \
         $d = New-Object System.Windows.Forms.OpenFileDialog; \
         $d.Title = '选择官方 Antigravity IDE 安装包或压缩包'; \
         $d.Filter = 'IDE 安装包 (*.exe;*.zip;*.7z)|*.exe;*.zip;*.7z|全部文件 (*.*)|*.*'; \
         if (Test-Path '{init}') {{ $d.InitialDirectory = '{init}' }}; \
         if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{ Write-Output $d.FileName }}"
    );
    run_sta_picker_powershell(&script).map(PathBuf::from)
}

fn run_sta_picker_powershell(script: &str) -> Option<String> {
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-WindowStyle", "Hidden", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 按需在当前用户桌面创建或刷新指向 AideRun.exe 的快捷方式（优先使用嵌入官方全彩图标）
pub fn create_desktop_shortcut(paths: &PortablePaths) -> Result<()> {
    let exe_str = paths.self_exe.display().to_string().replace('\'', "''");
    let root_str = paths.root.display().to_string().replace('\'', "''");
    let icon_str = format!("{exe_str}, 0");

    let ps_script = format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         $lnk = $ws.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Desktop')) 'Antigravity便携版.lnk')); \
         $lnk.TargetPath = '{exe_str}'; \
         $lnk.WorkingDirectory = '{root_str}'; \
         $lnk.Description = 'Antigravity IDE Portable (AideRun)'; \
         $lnk.IconLocation = '{icon_str}'; \
         $lnk.Save()"
    );

    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("调用 WScript.Shell 创建桌面快捷方式失败")?;

    if !output.status.success() {
        anyhow::bail!(
            "创建快捷方式失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sanitize_instance_name;

    #[test]
    fn sanitizes_instance_names_safely() {
        assert_eq!(sanitize_instance_name("work-01"), "work-01");
        assert_eq!(sanitize_instance_name(" ../evil\\dir "), "evildir");
        assert_eq!(sanitize_instance_name("   "), "");
    }
}
