// AideRun - Scoop 跨盘符移动自愈引擎（NTFS Junction 重建与 Shim 绝对路径重写）

use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

use crate::console::CREATE_NO_WINDOW;
use crate::paths::PortablePaths;

/// 自愈结果摘要
#[derive(Debug, Clone, Default)]
pub struct HealReport {
    pub needed: bool,
    pub is_ntfs: bool,
    pub junctions_repaired: usize,
    pub shims_rewritten: usize,
    pub warnings: Vec<String>,
}

/// 检测指定路径所在分区是否为支持 Junction 的 NTFS 文件系统
pub fn is_ntfs_volume(path: &Path) -> bool {
    let root_str = path
        .components()
        .next()
        .map(|c| format!("{}\\", c.as_os_str().to_string_lossy()))
        .unwrap_or_else(|| "C:\\".to_string());

    let wide_root: Vec<u16> = std::ffi::OsStr::new(&root_str)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut fs_name = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(
            wide_root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return false;
    }
    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..len]);
    name.eq_ignore_ascii_case("NTFS")
}

/// 检测目录移动或盘符变化，按需自动修复 tools/scoop 软链接与 shim 脚本
pub fn auto_heal_scoop_if_needed(paths: &PortablePaths, force: bool) -> Result<HealReport> {
    let mut report = HealReport {
        is_ntfs: is_ntfs_volume(&paths.root),
        ..Default::default()
    };

    if !paths.scoop_dir.is_dir() {
        return Ok(report);
    }

    let current_root_str = paths.root.display().to_string();
    let recorded = std::fs::read_to_string(&paths.root_marker).ok();
    let root_changed = recorded
        .as_deref()
        .map(|s| !s.trim().eq_ignore_ascii_case(&current_root_str))
        .unwrap_or(true);

    let has_broken_ref = has_stale_scoop_refs(paths);
    if !force && !root_changed && !has_broken_ref {
        return Ok(report);
    }
    report.needed = true;

    if !report.is_ntfs {
        report.warnings.push(
            "当前磁盘非 NTFS 格式（如 exFAT/FAT32），无法创建 Scoop Junction；启动器将自动回退挂载真实版本号目录。".to_string(),
        );
    } else {
        repair_scoop_junctions(paths, &mut report)?;
    }

    rewrite_scoop_shims(paths, &mut report)?;

    if let Some(parent) = paths.root_marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&paths.root_marker, format!("{current_root_str}\n"));

    Ok(report)
}

/// 检查是否存在失效的 current 链接或指向其他盘符的 shim 文件
pub fn has_stale_scoop_refs(paths: &PortablePaths) -> bool {
    let expected_prefix = paths.scoop_dir.display().to_string().to_ascii_lowercase();

    let apps_dir = paths.scoop_dir.join("apps");
    if let Ok(entries) = std::fs::read_dir(&apps_dir) {
        for entry in entries.flatten() {
            let app_path = entry.path();
            if !app_path.is_dir() {
                continue;
            }
            if app_path.file_name().and_then(|n| n.to_str()) == Some("scoop") {
                continue;
            }
            let current = app_path.join("current");
            if let Ok(target) = std::fs::read_link(&current) {
                let t_str = clean_win_link(&target).to_ascii_lowercase();
                if !t_str.starts_with(&expected_prefix) || !target.exists() {
                    return true;
                }
            }
        }
    }

    let shims_dir = paths.scoop_dir.join("shims");
    if let Ok(entries) = std::fs::read_dir(&shims_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext.to_ascii_lowercase().as_str(), "shim" | "cmd" | "ps1") {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Some(found) = find_scoop_prefix_in_text(&text) {
                        if !found.eq_ignore_ascii_case(&expected_prefix) {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

fn repair_scoop_junctions(paths: &PortablePaths, report: &mut HealReport) -> Result<()> {
    let apps_dir = paths.scoop_dir.join("apps");
    if !apps_dir.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&apps_dir)?.flatten() {
        let app_dir = entry.path();
        if !app_dir.is_dir() {
            continue;
        }
        let app_name = app_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if app_name.eq_ignore_ascii_case("scoop") {
            continue;
        }

        let current_link = app_dir.join("current");
        let target_ver_dir = choose_target_version_dir(&app_dir, &current_link);
        if let Some(ver_dir) = target_ver_dir {
            let needs_relink = match std::fs::read_link(&current_link) {
                Ok(old_target) => {
                    !clean_win_link(&old_target).eq_ignore_ascii_case(&ver_dir.display().to_string())
                        || !current_link.exists()
                }
                Err(_) => !current_link.exists(),
            };
            if needs_relink && recreate_junction(&current_link, &ver_dir).is_ok() {
                report.junctions_repaired += 1;
            }

            // 同步修复版本目录内部的 persist 嵌套 Junction（如 nodejs-lts/bin, python/Scripts）
            repair_nested_persist_junctions(paths, &app_name, &ver_dir, report);
        }
    }
    Ok(())
}

fn choose_target_version_dir(app_dir: &Path, current_link: &Path) -> Option<PathBuf> {
    if let Ok(old) = std::fs::read_link(current_link) {
        let cleaned = clean_win_link(&old);
        if let Some(ver_name) = Path::new(&cleaned).file_name() {
            let candidate = app_dir.join(ver_name);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir(app_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()) != Some("current"))
        .collect();
    versions.sort();
    versions.pop()
}

fn repair_nested_persist_junctions(
    paths: &PortablePaths,
    app_name: &str,
    ver_dir: &Path,
    report: &mut HealReport,
) {
    let persist_app = paths.scoop_dir.join("persist").join(app_name);
    if !persist_app.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(ver_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let item = entry.path();
        if let Ok(old_target) = std::fs::read_link(&item) {
            let old_str = clean_win_link(&old_target).to_ascii_lowercase();
            if old_str.contains("\\persist\\") {
                if let Some(leaf) = item.file_name() {
                    let desired = persist_app.join(leaf);
                    if desired.exists()
                        && !clean_win_link(&old_target)
                            .eq_ignore_ascii_case(&desired.display().to_string())
                    {
                        if recreate_junction(&item, &desired).is_ok() {
                            report.junctions_repaired += 1;
                        }
                    }
                }
            }
        }
    }
}

fn recreate_junction(link_path: &Path, target_dir: &Path) -> Result<()> {
    if link_path.symlink_metadata().is_ok() {
        // Windows 上 fs::remove_dir 仅移除 Junction 重解析点本身，不会删除目标文件
        let _ = std::fs::remove_dir(link_path);
    }
    let status = Command::new("cmd.exe")
        .args(["/C", "mklink", "/J"])
        .arg(link_path)
        .arg(target_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("调用 mklink /J 失败: {}", link_path.display()))?;
    if !status.status.success() {
        anyhow::bail!(
            "mklink /J 返回失败: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
    Ok(())
}

fn rewrite_scoop_shims(paths: &PortablePaths, report: &mut HealReport) -> Result<()> {
    let shims_dir = paths.scoop_dir.join("shims");
    if !shims_dir.is_dir() {
        return Ok(());
    }
    let new_scoop = paths.scoop_dir.display().to_string();

    for entry in std::fs::read_dir(&shims_dir)?.flatten() {
        let p = entry.path();
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "shim" | "cmd" | "ps1") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Some(updated) = replace_scoop_prefixes(&content, &new_scoop) {
            if updated != content && std::fs::write(&p, updated).is_ok() {
                report.shims_rewritten += 1;
            }
        }
    }
    Ok(())
}

/// 在脚本或 .shim 内容中提取形如 `X:\...\tools\scoop` 的绝对前缀
fn find_scoop_prefix_in_text(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("\\tools\\scoop")?;
    let bytes = text.as_bytes();
    let mut start = idx;
    while start >= 2 {
        if bytes[start - 1] == b':' && bytes[start - 2].is_ascii_alphabetic() {
            // 往前退两格拿到盘符起点，如 `D:\...`
            return Some(text[start - 2..idx + "\\tools\\scoop".len()].to_string());
        }
        if matches!(bytes[start - 1], b'"' | b'\'' | b'\n' | b'\r' | b'=') {
            break;
        }
        start -= 1;
    }
    None
}

/// 替换文本中全部出现的 `<盘符>:\...\tools\scoop` 为新的 `new_scoop`
fn replace_scoop_prefixes(text: &str, new_scoop: &str) -> Option<String> {
    let mut result = text.to_string();
    let mut changed = false;
    while let Some(found) = find_scoop_prefix_in_text(&result) {
        if found.eq_ignore_ascii_case(new_scoop) {
            break;
        }
        result = result.replace(&found, new_scoop);
        changed = true;
    }
    if changed {
        Some(result)
    } else {
        None
    }
}

fn clean_win_link(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_old_scoop_paths_in_shim_and_cmd() {
        let old_shim = "path = \"D:\\Antigravity IDE\\tools\\scoop\\apps\\git\\current\\cmd\\git.exe\"\n";
        let updated = replace_scoop_prefixes(old_shim, r"E:\USB\Antigravity IDE\tools\scoop").unwrap();
        assert_eq!(
            updated,
            "path = \"E:\\USB\\Antigravity IDE\\tools\\scoop\\apps\\git\\current\\cmd\\git.exe\"\n"
        );

        let old_cmd = "@rem D:\\Antigravity IDE\\tools\\scoop\\apps\\scoop\\current\\bin\\scoop.ps1\r\npwsh -file \"D:\\Antigravity IDE\\tools\\scoop\\apps\\scoop\\current\\bin\\scoop.ps1\" %*";
        let updated_cmd = replace_scoop_prefixes(old_cmd, r"F:\Work\tools\scoop").unwrap();
        assert!(updated_cmd.contains(r"F:\Work\tools\scoop\apps\scoop"));
        assert!(!updated_cmd.contains(r"D:\Antigravity IDE"));
    }
}

