// AideRun - 便携环境健康诊断与宿主机逃逸风险检查模块

use std::fmt::Write;

use anyhow::Result;

use crate::clean::{format_bytes, measure_path_size, scan_clean_targets, CleanLevel};
use crate::config::LauncherConfig;
use crate::paths::PortablePaths;
use crate::process::find_active_processes_in_root;
use crate::proxy::{check_proxy_reachable, read_config_json_proxy, read_windows_system_proxy};
use crate::relocate::detect_pending_relocation;
use crate::scoop_heal::is_ntfs_volume;
use crate::upgrade::{is_inno_setup_installer, read_local_versions};

/// 生成完整便携环境体检报告文本（供 CLI 输出与启动面板共享）
pub fn collect_doctor_report(paths: &PortablePaths, cfg: &LauncherConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "==================================================================");
    let _ = writeln!(out, "               AideRun 便携开发环境健康体检报告");
    let _ = writeln!(out, "==================================================================");

    let is_ntfs = is_ntfs_volume(&paths.root);
    let _ = writeln!(out, "1. 路径与磁盘文件系统:");
    let _ = writeln!(out, "   - 便携根目录  : {}", paths.root.display());
    let _ = writeln!(
        out,
        "   - 分区格式    : {} ({})",
        if is_ntfs { "NTFS" } else { "非 NTFS (如 exFAT/FAT32)" },
        if is_ntfs {
            "支持硬/软链接与原生零依赖组件"
        } else {
            "完全启用零软链接纯绿色原生组件，跨盘移动零影响"
        }
    );
    let _ = writeln!(
        out,
        "   - 便携骨架    : {}",
        if paths.needs_portable_bootstrap() {
            "[需部署] 缺失部分便携隔离目录，建议启动一次或执行 `AideRun.exe init`"
        } else {
            "[正常] data / user-data / extensions / profile / projects / tools 完整"
        }
    );
    if let Some(pending) = detect_pending_relocation(paths) {
        let source_note = if pending.from_marker { "" } else { "·配置反推" };
        let _ = writeln!(
            out,
            "   - 跨盘符迁移  : [待自愈{source_note}] 检测到旧目录 {} -> 当前 {}（启动 IDE/终端或运行 `AideRun.exe relocate` 将自动修复）",
            pending.recorded_root, pending.current_root
        );
    } else {
        let _ = writeln!(
            out,
            "   - 跨盘符迁移  : [一致] 当前根目录锚点正常，支持 U 盘换电脑盘符变化后无感自动重定位"
        );
    }

    let _ = writeln!(out, "\n2. Antigravity IDE 内核与代理状态:");
    if !paths.ide_exe.is_file() {
        let _ = writeln!(out, "   - 主程序      : [缺失] 未找到 Antigravity IDE.exe");
    } else if !paths.resources_dir.is_dir() && is_inno_setup_installer(&paths.ide_exe) {
        let _ = writeln!(
            out,
            "   - 主程序      : [待解包] 检测到 Antigravity IDE.exe 尚为官方安装向导，首次运行将自动无痕解包"
        );
    } else {
        let (ide_ver, vs_ver) = read_local_versions(paths);
        let _ = writeln!(
            out,
            "   - 主程序      : [正常] Antigravity IDE {} (VS Code {})",
            ide_ver.as_deref().unwrap_or("未知"),
            vs_ver.as_deref().unwrap_or("未知")
        );
    }

    let proxy_dll = paths.root.join("version.dll").is_file();
    let _ = writeln!(
        out,
        "   - 代理内核    : version.dll ({}) / config.json ({})",
        if proxy_dll { "已就绪" } else { "未启用" },
        if paths.proxy_config.is_file() { "存在" } else { "缺失" }
    );
    if let Some(sys) = read_windows_system_proxy() {
        let ok = check_proxy_reachable(&sys);
        let _ = writeln!(
            out,
            "   - 系统代理    : {} (端口连通性: {})",
            sys.all_proxy_url(),
            if ok { "通畅" } else { "未响应" }
        );
    } else {
        let _ = writeln!(out, "   - 系统代理    : 未开启");
    }
    if let Some(cfg_ep) = read_config_json_proxy(&paths.proxy_config) {
        let ok = check_proxy_reachable(&cfg_ep);
        let _ = writeln!(
            out,
            "   - 配置代理    : {} (端口连通性: {})",
            cfg_ep.all_proxy_url(),
            if ok { "通畅" } else { "未响应" }
        );
    }

    let _ = writeln!(out, "\n3. 原生零软链接组件状态 (tools/portable/):");
    let native_names: Vec<String> = std::fs::read_dir(&paths.portable_tools_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    if native_names.is_empty() {
        let _ = writeln!(out, "   - 已装组件    : 0 个（可在「组件管理」面板按需一键安装）");
    } else {
        let _ = writeln!(
            out,
            "   - 已装组件    : {} 个 [{}]",
            native_names.len(),
            native_names.join(", ")
        );
    }
    let _ = writeln!(out, "   - 环境变量    : 启动器自动将已装组件目录注入便携隔离 PATH，跨盘即插即用");

    let _ = writeln!(out, "\n4. 进程占用与可释放垃圾空间:");
    let running = find_active_processes_in_root(&paths.root);
    if running.is_empty() {
        let _ = writeln!(out, "   - 进程占用    : 无（可随时执行清理、脱敏或内核升级）");
    } else {
        let _ = writeln!(
            out,
            "   - 进程占用    : 当前目录有 {} 个进程正在运行:",
            running.len()
        );
        for p in &running {
            let _ = writeln!(
                out,
                "       * PID {:<6} {} ({})",
                p.pid,
                p.name,
                paths.rel_display(std::path::Path::new(&p.exe_path))
            );
        }
    }

    let (data_sz, _) = measure_path_size(&paths.data_dir);
    let clean_all = scan_clean_targets(paths, &cfg.cleanup, CleanLevel::All);
    let junk_sz: u64 = clean_all.iter().map(|t| t.size_bytes).sum();
    let _ = writeln!(out, "   - data 总占用 : {}", format_bytes(data_sz));
    let _ = writeln!(
        out,
        "   - 可瘦身垃圾  : {}（执行 `AideRun.exe clean --all` 可全部释放）",
        format_bytes(junk_sz)
    );

    let _ = writeln!(out, "\n5. 遗留旧脚本检查:");
    let legacy: Vec<&str> = [
        "Start-Portable.vbs",
        "Antigravity-Portable.bat",
        "Manage-Tools.bat",
        "Clean-Cache.bat",
        "Clean-Account-For-Packaging.bat",
    ]
    .into_iter()
    .filter(|f| paths.root.join(f).is_file())
    .collect();
    if legacy.is_empty() {
        let _ = writeln!(out, "   - 根目录整洁  : [极简] 无冗余 .vbs / .bat 残留脚本");
    } else {
        let _ = writeln!(
            out,
            "   - 旧版脚本    : 仍保留 {} 个旧脚本作为回退参考（确认稳定后可手动清理）",
            legacy.len()
        );
    }

    out
}

/// CLI 打印全方位便携健康体检
pub fn run_doctor(paths: &PortablePaths, cfg: &LauncherConfig) -> Result<()> {
    print!("{}", collect_doctor_report(paths, cfg));
    Ok(())
}
