// AideRun - 进程互斥感知的分级智能垃圾清理、组成明细统计与后台自清理模块

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use walkdir::WalkDir;

use crate::config::{CleanupConfig, LauncherConfig};
use crate::console::prompt_confirm;
use crate::paths::PortablePaths;
use crate::process::find_blocking_ide_processes;

const SMART_CLEAN_THRESHOLD_BYTES: u64 = 200 * 1024 * 1024;
const SMART_CLEAN_MIN_BYTES: u64 = 1024 * 1024;
const SMART_CLEAN_INTERVAL_SECS: i64 = 6 * 3600;

/// 清理深度等级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanLevel {
    /// 轻量：临时文件、运行日志、崩溃转储、代理日志
    Light,
    /// 日常推荐：含 Chromium/VSCode Web 缓存、Code Cache、GPU/Dawn 缓存、Crashpad 等
    Deep,
    /// 打包前彻底清理：含日常垃圾与 pip/npm/uv/go/cargo 下载与编译缓存
    All,
}

impl CleanLevel {
    pub fn label(&self) -> &'static str {
        match self {
            CleanLevel::Light => "轻量清理 (Temp/Logs/CrashDumps/ProxyLog)",
            CleanLevel::Deep => "一键清理日常垃圾 (Temp/Logs 与 IDE 运行缓存)",
            CleanLevel::All => "打包前彻底清理 (含日常垃圾与 pip/npm/uv/Go/Cargo 缓存)",
        }
    }
}

/// 垃圾项归类（日常清理 vs 打包专清）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanCategory {
    Daily,
    PackOnly,
}

/// 单个待清理条目的扫描结果
#[derive(Debug, Clone)]
pub struct CleanTarget {
    pub label: String,
    pub category: CleanCategory,
    pub path: PathBuf,
    pub is_dir: bool,
    pub recreate_empty_dir: bool,
    pub size_bytes: u64,
    pub file_count: u64,
}

/// 供面板 UI 渲染的垃圾组成明细项
#[derive(Debug, Clone, Serialize)]
pub struct CleanBreakdownItem {
    pub label: String,
    pub rel_path: String,
    pub category: &'static str,
    pub size_bytes: u64,
    pub size_fmt: String,
    pub file_count: u64,
}

/// 清理汇总报告
#[derive(Debug, Clone, Default)]
pub struct CleanSummary {
    #[allow(dead_code)]
    pub total_bytes_found: u64,
    pub bytes_freed: u64,
    pub items_cleaned: usize,
    pub skipped_locked: usize,
}

fn push_user_data_cache_specs(
    specs: &mut Vec<(String, PathBuf, bool, CleanCategory)>,
    ud: &Path,
    inst_tag: Option<&str>,
) {
    let prefix = inst_tag.map(|n| format!("分身[{n}] ")).unwrap_or_default();
    for (label, rel) in [
        ("IDE 网络缓存 (Cache)", "Cache"),
        ("IDE 字节码缓存 (CachedData)", "CachedData"),
        ("IDE V8 代码缓存 (Code Cache)", "Code Cache"),
        ("GPU 磁盘缓存 (GPUCache)", "GPUCache"),
        ("Dawn Graphite 缓存", "DawnGraphiteCache"),
        ("Dawn WebGPU 缓存", "DawnWebGPUCache"),
        ("Crashpad 转储", "Crashpad"),
        ("Service Worker 缓存", "Service Worker"),
        ("WebStorage 缓存", "WebStorage"),
        ("Blob 存储缓存", "blob_storage"),
        ("共享字典缓存", "Shared Dictionary"),
        ("已缓存扩展包 (CachedExtensionVSIXs)", "CachedExtensionVSIXs"),
        ("IDE 内部日志 (user-data/logs)", "logs"),
    ] {
        specs.push((
            format!("{prefix}{label}"),
            ud.join(rel),
            true,
            CleanCategory::Daily,
        ));
    }
}

/// 收集指定清理级别下的所有目标及其占用空间（CLI 兼容默认包含 WebView2 目录）
pub fn scan_clean_targets(
    paths: &PortablePaths,
    cleanup_cfg: &CleanupConfig,
    level: CleanLevel,
) -> Vec<CleanTarget> {
    scan_clean_targets_with_opts(paths, cleanup_cfg, level, true)
}

/// 收集指定清理级别下的所有目标及其占用空间（控制面板调用时自动跳过当前运行中的 WebView2 目录）
pub fn scan_clean_targets_with_opts(
    paths: &PortablePaths,
    cleanup_cfg: &CleanupConfig,
    level: CleanLevel,
    include_webview_cache: bool,
) -> Vec<CleanTarget> {
    let preserve_set: HashSet<String> = cleanup_cfg
        .preserve_paths
        .iter()
        .map(|s| normalize_rel_key(s))
        .collect();

    let mut specs: Vec<(String, PathBuf, bool, CleanCategory)> = vec![
        ("临时文件 (cache/temp)".into(), paths.temp_dir.clone(), true, CleanCategory::Daily),
        ("运行日志 (cache/logs)".into(), paths.logs_dir.clone(), true, CleanCategory::Daily),
        ("崩溃转储 (cache/crash-dumps)".into(), paths.crash_dumps_dir.clone(), true, CleanCategory::Daily),
        ("调试日志 (debug.log)".into(), paths.root.join("debug.log"), false, CleanCategory::Daily),
    ];

    // 根目录代理日志 proxy-*.log（保留 load-notify-success.marker 等标记文件）
    let root_logs = paths.root.join("logs");
    if root_logs.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&root_logs) {
            for entry in entries.flatten() {
                let p = entry.path();
                let name = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if p.is_file() && name.ends_with(".log") {
                    specs.push(("代理日志 (logs/*.log)".into(), p, false, CleanCategory::Daily));
                }
            }
        }
    }

    if matches!(level, CleanLevel::Deep | CleanLevel::All) {
        push_user_data_cache_specs(&mut specs, &paths.user_data_dir, None);

        // 同步纳入多开隔离分身的日常缓存
        let instances_dir = paths.data_dir.join("instances");
        if instances_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&instances_dir) {
                for entry in entries.flatten() {
                    let inst_path = entry.path();
                    if inst_path.is_dir() {
                        if let Some(inst_name) = inst_path.file_name().and_then(|n| n.to_str()) {
                            let inst_ud = inst_path.join("user-data");
                            if inst_ud.is_dir() {
                                push_user_data_cache_specs(&mut specs, &inst_ud, Some(inst_name));
                            }
                        }
                    }
                }
            }
        }

        specs.push((
            "Direct3D 着色器缓存 (D3DSCache)".into(),
            paths.appdata_local.join("D3DSCache"),
            false,
            CleanCategory::Daily,
        ));
        specs.push((
            "Profile 通用缓存 (profile/.cache)".into(),
            paths.profile_dir.join(".cache"),
            false,
            CleanCategory::Daily,
        ));
        if include_webview_cache {
            specs.push((
                "启动器面板 WebView2 缓存 (cache/webview2)".into(),
                paths.cache_dir.join("webview2"),
                true,
                CleanCategory::Daily,
            ));
        }
    }

    if level == CleanLevel::All {
        specs.push(("Pip 下载缓存 (cache/pip)".into(), paths.pip_cache_dir.clone(), true, CleanCategory::PackOnly));
        specs.push(("Npm 缓存 (cache/npm)".into(), paths.npm_cache_dir.clone(), true, CleanCategory::PackOnly));
        specs.push(("UV 包缓存 (cache/uv)".into(), paths.uv_cache_dir.clone(), true, CleanCategory::PackOnly));
        specs.push(("Go 编译缓存 (cache/go-build)".into(), paths.go_cache_dir.clone(), true, CleanCategory::PackOnly));
        specs.push((
            "Go 模块缓存 (cache/go-mod)".into(),
            paths.cache_dir.join("go-mod"),
            false,
            CleanCategory::PackOnly,
        ));
        specs.push((
            "XDG 缓存 (cache/xdg)".into(),
            paths.cache_dir.join("xdg"),
            false,
            CleanCategory::PackOnly,
        ));
        specs.push((
            "Rust 编译产物缓存 (cache/cargo-target)".into(),
            paths.cache_dir.join("cargo-target"),
            false,
            CleanCategory::PackOnly,
        ));
        specs.push((
            "Cargo Registry 下载缓存".into(),
            paths.cargo_home.join("registry").join("cache"),
            false,
            CleanCategory::PackOnly,
        ));
        specs.push((
            "Cargo Registry 解压源码".into(),
            paths.cargo_home.join("registry").join("src"),
            false,
            CleanCategory::PackOnly,
        ));
        specs.push((
            "跨盘符迁移回滚快照 (cache/_relocate_backup)".into(),
            paths.cache_dir.join("_relocate_backup"),
            false,
            CleanCategory::PackOnly,
        ));
    }

    let mut targets = Vec::new();
    let mut seen_paths = HashSet::new();

    for (label, path, recreate, category) in specs {
        if is_preserved(paths, &path, &preserve_set) || !path.exists() {
            continue;
        }
        let key = path.display().to_string().to_ascii_lowercase();
        if !seen_paths.insert(key) {
            continue;
        }
        let (size_bytes, file_count) = measure_path_size(&path);
        if size_bytes > 0 || file_count > 0 {
            targets.push(CleanTarget {
                label,
                category,
                is_dir: path.is_dir(),
                recreate_empty_dir: recreate,
                path,
                size_bytes,
                file_count,
            });
        }
    }

    for extra_rel in &cleanup_cfg.extra_clean_paths {
        let p = if Path::new(extra_rel).is_absolute() {
            PathBuf::from(extra_rel)
        } else {
            paths.root.join(extra_rel)
        };
        if !p.exists() || is_preserved(paths, &p, &preserve_set) {
            continue;
        }
        // 安全防线：绝不允许通过自定义配置误删根目录核心资产
        if p == paths.root || p == paths.projects_dir || p == paths.data_dir || p == paths.tools_dir {
            continue;
        }
        let key = p.display().to_string().to_ascii_lowercase();
        if !seen_paths.insert(key) {
            continue;
        }
        let (size_bytes, file_count) = measure_path_size(&p);
        if size_bytes > 0 || file_count > 0 {
            targets.push(CleanTarget {
                label: format!("自定义规则 ({extra_rel})"),
                category: CleanCategory::Daily,
                is_dir: p.is_dir(),
                recreate_empty_dir: false,
                path: p,
                size_bytes,
                file_count,
            });
        }
    }

    targets.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.label.cmp(&b.label))
    });
    targets
}

/// 生成供控制面板展示的垃圾组成明细、日常可释放空间与打包彻底清理总空间（排除面板自身 WebView2 目录）
pub fn scan_clean_breakdown(
    paths: &PortablePaths,
    cleanup_cfg: &CleanupConfig,
) -> (String, String, Vec<CleanBreakdownItem>) {
    let targets = scan_clean_targets_with_opts(paths, cleanup_cfg, CleanLevel::All, false);
    let mut daily_bytes = 0u64;
    let mut total_bytes = 0u64;
    let mut items = Vec::with_capacity(targets.len());

    for t in targets {
        total_bytes += t.size_bytes;
        if t.category == CleanCategory::Daily {
            daily_bytes += t.size_bytes;
        }
        items.push(CleanBreakdownItem {
            label: t.label,
            rel_path: paths.rel_display(&t.path),
            category: match t.category {
                CleanCategory::Daily => "日常",
                CleanCategory::PackOnly => "打包",
            },
            size_bytes: t.size_bytes,
            size_fmt: format_bytes(t.size_bytes),
            file_count: t.file_count,
        });
    }

    (format_bytes(daily_bytes), format_bytes(total_bytes), items)
}

/// 执行智能清理：先检测进程锁并预览体积，确认后安全移除目标
pub fn run_clean(
    paths: &PortablePaths,
    cleanup_cfg: &CleanupConfig,
    level: CleanLevel,
    dry_run: bool,
    auto_yes: bool,
) -> Result<CleanSummary> {
    run_clean_with_opts(paths, cleanup_cfg, level, dry_run, auto_yes, true, false)
}

/// 支持控制面板保护 WebView2 目录及智能跳过占位锁的扩展清理接口
pub fn run_clean_with_opts(
    paths: &PortablePaths,
    cleanup_cfg: &CleanupConfig,
    level: CleanLevel,
    dry_run: bool,
    auto_yes: bool,
    include_webview_cache: bool,
    skip_if_ide_running: bool,
) -> Result<CleanSummary> {
    if !dry_run {
        let blockers = find_blocking_ide_processes(&paths.root);
        if !blockers.is_empty() {
            if skip_if_ide_running {
                return Ok(CleanSummary {
                    skipped_locked: blockers.len(),
                    ..Default::default()
                });
            }
            println!("[错误] 检测到以下当前便携目录进程正在运行，为防止损坏数据已终止清理：");
            for proc_info in &blockers {
                println!("  - PID {:<6} {}", proc_info.pid, proc_info.name);
            }
            anyhow::bail!("检测到 Antigravity IDE 正在运行，请先关闭 IDE 窗口后再执行清理。");
        }
    }

    let targets = scan_clean_targets_with_opts(paths, cleanup_cfg, level, include_webview_cache);
    let total_bytes: u64 = targets.iter().map(|t| t.size_bytes).sum();
    let mut summary = CleanSummary {
        total_bytes_found: total_bytes,
        ..Default::default()
    };

    println!("=== {} ===", level.label());
    if targets.is_empty() {
        println!("[√] 当前非常干净，未发现可清理的垃圾或缓存文件。");
        return Ok(summary);
    }

    for target in &targets {
        println!(
            "  [{:>9}] {:<40} ({} 文件) -> {}",
            format_bytes(target.size_bytes),
            target.label,
            target.file_count,
            paths.rel_display(&target.path)
        );
    }
    println!(
        "共计发现 {} 项，可释放空间约 {}",
        targets.len(),
        format_bytes(total_bytes)
    );

    if dry_run {
        println!("[Dry-Run] 仅统计预览，未删除任何文件。");
        return Ok(summary);
    }

    if !auto_yes && !prompt_confirm("确认清理以上缓存与临时文件？[y/N]: ", false) {
        println!("[已取消清理]");
        return Ok(summary);
    }

    for target in &targets {
        let res = if target.is_dir {
            clean_directory_contents(&target.path, target.recreate_empty_dir)
        } else {
            std::fs::remove_file(&target.path)
        };
        match res {
            Ok(()) => {
                summary.bytes_freed += target.size_bytes;
                summary.items_cleaned += 1;
            }
            Err(err) => {
                summary.skipped_locked += 1;
                println!("  [跳过占用] {} ({err})", paths.rel_display(&target.path));
            }
        }
    }

    println!(
        "[√] 清理完成！已释放约 {}（成功 {} 项，跳过占用 {} 项）。settings.json、扩展插件与 projects 代码完好保留。",
        format_bytes(summary.bytes_freed),
        summary.items_cleaned,
        summary.skipped_locked
    );
    Ok(summary)
}

/// 当用户勾选「程序智能清理」时：IDE 空闲且达到容量阈值（>=200MB）、距上次超 6 小时或立即触发条件时静默清理日常垃圾
pub fn run_smart_daily_clean_if_due(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    force_now: bool,
) -> Result<Option<CleanSummary>> {
    if !cfg.launch.smart_clean {
        return Ok(None);
    }
    if !find_blocking_ide_processes(&paths.root).is_empty() {
        return Ok(None);
    }

    let targets = scan_clean_targets_with_opts(paths, &cfg.cleanup, CleanLevel::Deep, false);
    let total_bytes: u64 = targets.iter().map(|t| t.size_bytes).sum();
    if total_bytes == 0 {
        return Ok(None);
    }

    let ts_path = paths.cache_dir.join(".smart_clean_ts");
    let now = Utc::now().timestamp();
    let last_ts: i64 = std::fs::read_to_string(&ts_path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(last_ts);

    let should_run = force_now
        || total_bytes >= SMART_CLEAN_THRESHOLD_BYTES
        || (total_bytes >= SMART_CLEAN_MIN_BYTES && elapsed >= SMART_CLEAN_INTERVAL_SECS);
    if !should_run {
        return Ok(None);
    }

    let sum = run_clean_with_opts(
        paths,
        &cfg.cleanup,
        CleanLevel::Deep,
        false,
        true,
        false,
        true,
    )?;
    if sum.items_cleaned > 0 || sum.bytes_freed > 0 {
        let _ = std::fs::create_dir_all(&paths.cache_dir);
        let _ = std::fs::write(&ts_path, now.to_string());
    }
    Ok(Some(sum))
}

fn clean_directory_contents(dir: &Path, recreate_empty: bool) -> std::io::Result<()> {
    if recreate_empty {
        if let Err(_) = std::fs::remove_dir_all(dir) {
            // 部分子文件被杀软或索引占用时，尽量逐项删除
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        let _ = std::fs::remove_dir_all(p);
                    } else {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
        }
        std::fs::create_dir_all(dir)?;
        Ok(())
    } else {
        std::fs::remove_dir_all(dir)
    }
}

/// 统计目录或文件字节大小与文件数
pub fn measure_path_size(path: &Path) -> (u64, u64) {
    if path.is_file() {
        let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
        return (bytes, 1);
    }
    if !path.is_dir() {
        return (0, 0);
    }
    let mut bytes = 0u64;
    let mut files = 0u64;
    for entry in WalkDir::new(path).into_iter().flatten() {
        if entry.file_type().is_file() {
            files += 1;
            if let Ok(meta) = entry.metadata() {
                bytes += meta.len();
            }
        }
    }
    (bytes, files)
}

/// 格式化人类可读的存储容量
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn normalize_rel_key(s: &str) -> String {
    s.trim()
        .replace('/', "\\")
        .trim_start_matches(".\\")
        .to_ascii_lowercase()
}

fn is_preserved(paths: &PortablePaths, target: &Path, preserve_set: &HashSet<String>) -> bool {
    let rel = paths.rel_display(target).to_ascii_lowercase();
    preserve_set.contains(&rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_scans_without_deleting_files() {
        let tmp = std::env::temp_dir().join(format!("aiderun_clean_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let paths = PortablePaths::from_root(tmp.clone(), tmp.join("AideRun.exe"));
        paths.ensure_skeleton().unwrap();

        let sample_log = paths.logs_dir.join("test.log");
        std::fs::write(&sample_log, vec![b'x'; 2048]).unwrap();
        let sample_gpu = paths.user_data_dir.join("GPUCache").join("data_0");
        std::fs::create_dir_all(sample_gpu.parent().unwrap()).unwrap();
        std::fs::write(&sample_gpu, vec![b'y'; 4096]).unwrap();

        let summary = run_clean(&paths, &CleanupConfig::default(), CleanLevel::Deep, true, true).unwrap();
        assert_eq!(summary.total_bytes_found, 6144);
        assert!(sample_log.exists());
        assert!(sample_gpu.exists());

        let freed = run_clean(&paths, &CleanupConfig::default(), CleanLevel::Deep, false, true).unwrap();
        assert_eq!(freed.bytes_freed, 6144);
        assert!(!sample_log.exists());
        assert!(!sample_gpu.exists());
        assert!(paths.logs_dir.is_dir());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn gui_scan_excludes_webview2_and_sorts_descending() {
        let tmp = std::env::temp_dir().join(format!("aiderun_clean_gui_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let paths = PortablePaths::from_root(tmp.clone(), tmp.join("AideRun.exe"));
        paths.ensure_skeleton().unwrap();

        let webview_file = paths.cache_dir.join("webview2").join("EBWebView").join("data.bin");
        std::fs::create_dir_all(webview_file.parent().unwrap()).unwrap();
        std::fs::write(&webview_file, vec![b'w'; 8192]).unwrap();

        let small_log = paths.logs_dir.join("small.log");
        std::fs::write(&small_log, vec![b'a'; 512]).unwrap();

        let big_pip = paths.pip_cache_dir.join("big.whl");
        std::fs::write(&big_pip, vec![b'b'; 4096]).unwrap();

        let targets = scan_clean_targets_with_opts(&paths, &CleanupConfig::default(), CleanLevel::All, false);
        assert!(targets.iter().all(|t| !t.path.ends_with("webview2")));
        assert_eq!(targets[0].size_bytes, 4096);
        assert_eq!(targets[0].category, CleanCategory::PackOnly);
        assert_eq!(targets[1].size_bytes, 512);
        assert_eq!(targets[1].category, CleanCategory::Daily);

        let _ = run_clean_with_opts(&paths, &CleanupConfig::default(), CleanLevel::All, false, true, false, false).unwrap();
        assert!(webview_file.exists());
        assert!(!small_log.exists());
        assert!(!big_pip.exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

