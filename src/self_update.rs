// AideRun - 启动器本体 GitHub 远端检测、静默影子热替换自升级与重启衔接模块

use std::cmp::Ordering;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use crate::console::CREATE_NO_WINDOW;
use crate::net::{download_file_with_progress, http_get_text};
use crate::upgrade::compare_semver;

pub const GITHUB_REPO_URL: &str = "https://github.com/ZhangSir9901/AideRun";
const RAW_UPDATE_JSON_URL: &str =
    "https://raw.githubusercontent.com/ZhangSir9901/AideRun/main/Release/update.json";
const RAW_GITHUB_PREFIX: &str = "https://raw.githubusercontent.com/";

/// GitHub Release/update.json 清单模型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteSelfUpdateManifest {
    pub version: String,
    pub url: String,
}

/// 启动器自身更新检测结果
#[derive(Debug, Clone, Serialize)]
pub struct SelfUpdateCheckResult {
    pub local_version: String,
    pub remote_version: String,
    pub download_url: String,
    pub source_label: String,
    pub has_update: bool,
}

/// 使用系统默认浏览器打开 AideRun GitHub 开源仓库页面（无控制台黑窗）
pub fn open_github_repository() -> Result<()> {
    Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", GITHUB_REPO_URL])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("调起默认浏览器打开 GitHub 仓库失败")?;
    Ok(())
}

/// 为当前可执行文件路径附加扩展后缀（保留原 .exe，生成如 AideRun.exe.old）
pub fn append_exe_suffix(exe_path: &Path, suffix: &str) -> PathBuf {
    let mut os = exe_path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// 启动时自动清理上一次热替换遗留的 .old / .new 文件
pub fn cleanup_old_self_binaries() {
    let Ok(cur_exe) = std::env::current_exe() else {
        return;
    };
    for suffix in [".old", ".new"] {
        let candidate = append_exe_suffix(&cur_exe, suffix);
        if candidate.is_file() {
            for _ in 0..5 {
                if std::fs::remove_file(&candidate).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        }
    }
}

/// 在自升级重启后先等待旧进程 PID 完全退出释放窗口和文件锁，再清理 .old 文件
pub fn wait_for_old_pid_and_cleanup(old_pid: u32) {
    if old_pid > 0 && old_pid != std::process::id() {
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, old_pid);
            if !handle.is_null() {
                WaitForSingleObject(handle, 4000);
                CloseHandle(handle);
            }
        }
    }
    cleanup_old_self_binaries();
}

/// 解析 GitHub Release/update.json 文本（兼容 UTF-8 BOM）
pub fn parse_self_update_manifest(raw: &str) -> Result<RemoteSelfUpdateManifest> {
    let clean = raw.trim().trim_start_matches('\u{FEFF}');
    let mut manifest: RemoteSelfUpdateManifest =
        serde_json::from_str(clean).context("解析 update.json 格式失败")?;
    manifest.version = manifest.version.trim().trim_start_matches('v').to_string();
    manifest.url = manifest.url.trim().to_string();
    if manifest.version.is_empty() || manifest.url.is_empty() {
        anyhow::bail!("update.json 缺少有效的 version 或 url 字段");
    }
    if !manifest.url.starts_with("https://") {
        anyhow::bail!("update.json 下载地址必须为 HTTPS 协议: {}", manifest.url);
    }
    Ok(manifest)
}

fn cache_bust_url(url: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}t={ts}")
}

/// 构造主源与国内镜像回退 URL 列表（用于 manifest 检查和 EXE 下载）
pub fn build_mirror_candidates(primary_url: &str) -> Vec<(String, String)> {
    let mut list = vec![(primary_url.to_string(), "GitHub Raw".to_string())];
    if let Some(rest) = primary_url.strip_prefix(RAW_GITHUB_PREFIX) {
        list.push((
            format!("https://ghproxy.net/{RAW_GITHUB_PREFIX}{rest}"),
            "ghproxy.net 加速镜像".to_string(),
        ));
        list.push((
            format!("https://raw.kkgithub.com/{rest}"),
            "kkgithub 加速镜像".to_string(),
        ));
    } else if primary_url.starts_with("https://ghproxy.net/") {
        if let Some(orig) = primary_url.strip_prefix("https://ghproxy.net/") {
            list.push((orig.to_string(), "GitHub Raw 直连".to_string()));
        }
    }
    list
}

/// 向远端 Release/update.json 检测 AideRun 本体是否有新版本
pub fn check_self_update() -> Result<SelfUpdateCheckResult> {
    let local_version = env!("CARGO_PKG_VERSION").to_string();
    let mut errors = Vec::<String>::new();

    for (base_url, label) in build_mirror_candidates(RAW_UPDATE_JSON_URL) {
        let probe_url = cache_bust_url(&base_url);
        match http_get_text(&probe_url, 7).and_then(|txt| parse_self_update_manifest(&txt)) {
            Ok(manifest) => {
                let has_update =
                    compare_semver(&manifest.version, &local_version) == Ordering::Greater;
                return Ok(SelfUpdateCheckResult {
                    local_version,
                    remote_version: manifest.version,
                    download_url: manifest.url,
                    source_label: label,
                    has_update,
                });
            }
            Err(err) => errors.push(format!("{label}: {err:#}")),
        }
    }

    anyhow::bail!("检测 AideRun 远端更新失败：{}", errors.join(" | "))
}

/// 校验下载目标是否为合法的 Windows PE 可执行文件（大于 256KB 且以 MZ 开头）
pub fn is_valid_pe_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() < 256 * 1024 {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).is_ok() && &magic == b"MZ"
}

/// 下载新版 AideRun.exe 并执行 Windows 影子热替换（失败自动回滚保护当前程序）
pub fn download_and_swap_self<F>(primary_url: &str, mut on_progress: F) -> Result<()>
where
    F: FnMut(u8, &str),
{
    let cur_exe = std::env::current_exe().context("获取当前 AideRun.exe 路径失败")?;
    let new_exe = append_exe_suffix(&cur_exe, ".new");
    let old_exe = append_exe_suffix(&cur_exe, ".old");

    let _ = std::fs::remove_file(&new_exe);
    let _ = std::fs::remove_file(&old_exe);

    let candidates = build_mirror_candidates(primary_url);
    let mut downloaded_ok = false;
    let mut download_errors = Vec::<String>::new();

    for (idx, (cand_url, label)) in candidates.iter().enumerate() {
        let req_url = cache_bust_url(cand_url);
        on_progress(
            5,
            &format!(
                "正在连接更新源 ({}/{})：{}...",
                idx + 1,
                candidates.len(),
                label
            ),
        );

        let dl_res = download_file_with_progress(&req_url, &new_exe, |done, total, speed_bps| {
            let pct = match total {
                Some(t) if t > 0 => 8 + (((done as f64 / t as f64).clamp(0.0, 1.0)) * 80.0).round() as u8,
                _ => 8 + (((done as f64 / (4.0 * 1024.0 * 1024.0)).min(0.95)) * 80.0).round() as u8,
            };
            let mb_done = done as f64 / (1024.0 * 1024.0);
            let msg = match total {
                Some(t) if t > 0 => format!(
                    "正在下载新版 AideRun ({:.1} / {:.1} MB · {:.0} KB/s)",
                    mb_done,
                    t as f64 / (1024.0 * 1024.0),
                    (speed_bps / 1024.0).max(0.0)
                ),
                _ => format!("正在下载新版 AideRun ({:.1} MB)...", mb_done),
            };
            on_progress(pct.min(88), &msg);
        });

        match dl_res {
            Ok(()) if is_valid_pe_executable(&new_exe) => {
                downloaded_ok = true;
                break;
            }
            Ok(()) => {
                let _ = std::fs::remove_file(&new_exe);
                download_errors.push(format!("{label}: 返回内容非有效 Windows EXE（可能被镜像拦截）"));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&new_exe);
                download_errors.push(format!("{label}: {err:#}"));
            }
        }
    }

    if !downloaded_ok {
        anyhow::bail!("下载新版 AideRun.exe 失败：{}", download_errors.join(" | "));
    }

    on_progress(92, "校验通过，正在执行无感影子热替换...");

    std::fs::rename(&cur_exe, &old_exe).context("将运行中旧版重命名为 .old 失败")?;
    if let Err(err) = std::fs::rename(&new_exe, &cur_exe) {
        let _ = std::fs::rename(&old_exe, &cur_exe);
        return Err(err).context("替换新版 AideRun.exe 失败，已自动回滚旧版");
    }

    on_progress(100, "热替换完成，即将无缝重启新版 AideRun...");
    Ok(())
}

/// 携带当前旧进程 PID 启动刚完成替换的新版 AideRun.exe
pub fn spawn_new_self_after_update() -> Result<()> {
    let cur_exe = std::env::current_exe().context("定位当前可执行文件路径失败")?;
    let pid_str = std::process::id().to_string();
    Command::new(&cur_exe)
        .args(["--wait-pid", &pid_str])
        .spawn()
        .with_context(|| format!("启动新版启动器失败: {}", cur_exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_and_appends_exe_suffix_cleanly() {
        let raw = "\u{FEFF}{\"version\": \"v1.3.2\", \"url\": \"https://raw.githubusercontent.com/ZhangSir9901/AideRun/main/Release/AideRun.exe\"}";
        let m = parse_self_update_manifest(raw).unwrap();
        assert_eq!(m.version, "1.3.2");
        assert!(m.url.starts_with("https://raw.githubusercontent.com/"));

        let base = Path::new(r"D:\Antigravity IDE\AideRun.exe");
        assert_eq!(
            append_exe_suffix(base, ".old"),
            Path::new(r"D:\Antigravity IDE\AideRun.exe.old")
        );

        let mirrors = build_mirror_candidates(&m.url);
        assert_eq!(mirrors.len(), 3);
        assert!(mirrors[1].0.contains("ghproxy.net"));
        assert!(mirrors[2].0.contains("raw.kkgithub.com"));
    }

    #[test]
    fn validates_pe_magic_and_minimum_size() {
        let tmp = std::env::temp_dir().join(format!("aiderun_pe_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let fake_html = tmp.join("bad.exe");
        std::fs::write(&fake_html, b"<html>404 Not Found</html>").unwrap();
        assert!(!is_valid_pe_executable(&fake_html));

        let valid_pe = tmp.join("good.exe");
        let mut buf = vec![0u8; 300 * 1024];
        buf[0] = b'M';
        buf[1] = b'Z';
        std::fs::write(&valid_pe, &buf).unwrap();
        assert!(is_valid_pe_executable(&valid_pe));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

