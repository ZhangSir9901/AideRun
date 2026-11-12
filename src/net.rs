// AideRun - 网络请求、文件下载（含静默实时进度监控）与压缩包通用解压辅助模块

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows_sys::Win32::System::Console::GetConsoleWindow;

use crate::console::CREATE_NO_WINDOW;
use crate::env::resolve_scoop_app_root;
use crate::paths::PortablePaths;

/// 通过系统 curl.exe 获取文本内容（自动继承进程 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY）
pub fn http_get_text(url: &str, timeout_secs: u64) -> Result<String> {
    let output = Command::new("curl.exe")
        .args([
            "-fsSL",
            "--connect-timeout",
            "8",
            "--max-time",
            &timeout_secs.to_string(),
            "-A",
            "AideRun/1.0",
            url,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("调用 curl.exe 访问失败: {url}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("HTTP 请求失败 ({url}): {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 探测指定 URL 是否可访问（HEAD 请求返回 2xx/3xx）
pub fn http_head_ok(url: &str) -> bool {
    let Ok(output) = Command::new("curl.exe")
        .args(["-fsSIL", "--connect-timeout", "5", "--max-time", "10", url])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return false;
    };
    output.status.success()
}

/// 下载文件到指定路径（临时文件写入成功后原子重命名；无控制台宿主时强制静默避免闪烁黑框）
pub fn download_file(url: &str, dest: &Path, show_progress: bool) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_dest = dest.with_extension("download.part");
    let _ = std::fs::remove_file(&tmp_dest);

    let has_console = unsafe { !GetConsoleWindow().is_null() };
    let render_console_bar = show_progress && has_console;

    let mut cmd = Command::new("curl.exe");
    cmd.arg("-fL").arg("--connect-timeout").arg("10");
    if render_console_bar {
        cmd.arg("--progress-bar");
    } else {
        cmd.arg("-sS");
    }
    cmd.args(["-A", "AideRun/1.0", "-o"])
        .arg(&tmp_dest)
        .arg(url);

    if !render_console_bar {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let status = cmd
        .status()
        .with_context(|| format!("执行 curl.exe 下载失败: {url}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp_dest);
        anyhow::bail!("下载失败 ({url})，退出码: {:?}", status.code());
    }
    if dest.exists() {
        let _ = std::fs::remove_file(dest);
    }
    std::fs::rename(&tmp_dest, dest)?;
    Ok(())
}

/// 从 curl HEAD 响应头文本中解析最终跳转目标的 Content-Length 字节数
pub fn parse_content_length_from_headers(headers: &str) -> Option<u64> {
    let mut final_length = None;
    for raw_line in headers.lines() {
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(bytes) = rest.trim().parse::<u64>() {
                if bytes > 0 {
                    final_length = Some(bytes);
                }
            }
        }
    }
    final_length
}

/// 通过 HEAD 请求探测远端下载文件总大小（失败或采用分块传输时返回 None）
pub fn probe_content_length(url: &str) -> Option<u64> {
    let output = Command::new("curl.exe")
        .args(["-fsSIL", "--connect-timeout", "8", "-A", "AideRun/1.0", url])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let headers = String::from_utf8_lossy(&output.stdout);
    parse_content_length_from_headers(&headers)
}

/// 静默后台下载文件并按固定节拍回调实时已下载字节、预估总大小与瞬时均值速率（bytes/sec）
pub fn download_file_with_progress<F>(url: &str, dest: &Path, mut on_progress: F) -> Result<()>
where
    F: FnMut(u64, Option<u64>, f64),
{
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_dest = dest.with_extension("download.part");
    let _ = std::fs::remove_file(&tmp_dest);

    let total_bytes = probe_content_length(url);
    on_progress(0, total_bytes, 0.0);

    let started = Instant::now();
    let mut child = Command::new("curl.exe")
        .args([
            "-fL",
            "--connect-timeout",
            "15",
            "-sS",
            "-A",
            "AideRun/1.0",
            "-o",
        ])
        .arg(&tmp_dest)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("启动 curl.exe 下载失败: {url}"))?;

    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let mut err_buf = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut err_buf);
                }
                let _ = std::fs::remove_file(&tmp_dest);
                let detail = err_buf.trim();
                if detail.is_empty() {
                    anyhow::bail!("下载失败 ({url})，退出码: {:?}", status.code());
                }
                anyhow::bail!("下载失败 ({url}): {detail}");
            }
            break;
        }
        let downloaded = std::fs::metadata(&tmp_dest).map(|m| m.len()).unwrap_or(0);
        let elapsed_secs = started.elapsed().as_secs_f64().max(0.1);
        let speed_bps = downloaded as f64 / elapsed_secs;
        on_progress(downloaded, total_bytes, speed_bps);
        std::thread::sleep(Duration::from_millis(180));
    }

    let final_bytes = std::fs::metadata(&tmp_dest).map(|m| m.len()).unwrap_or(0);
    if final_bytes == 0 {
        let _ = std::fs::remove_file(&tmp_dest);
        anyhow::bail!("下载完成但目标文件为空: {url}");
    }
    let elapsed_secs = started.elapsed().as_secs_f64().max(0.1);
    on_progress(
        final_bytes,
        total_bytes.or(Some(final_bytes)),
        final_bytes as f64 / elapsed_secs,
    );

    if dest.exists() {
        let _ = std::fs::remove_file(dest);
    }
    std::fs::rename(&tmp_dest, dest)?;
    Ok(())
}

/// 定位本地可用的 7z.exe / 7zr.exe
pub fn find_7zip_exe(paths: &PortablePaths) -> Option<PathBuf> {
    let candidates = [
        paths.portable_tools_dir.join("7zip").join("7zr.exe"),
        paths.portable_tools_dir.join("7zip").join("7z.exe"),
        paths.tools_dir.join("7zr.exe"),
        paths.tools_dir.join("7z.exe"),
    ];
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    let scoop_7zip = paths.scoop_dir.join("apps").join("7zip");
    if let Some(root) = resolve_scoop_app_root(&scoop_7zip) {
        let exe = root.join("7z.exe");
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

/// 解压 .zip / .7z / .tar.gz 压缩包至目标目录，并自动剥离多余的单层嵌套外壳文件夹
pub fn extract_archive_clean(
    archive: &Path,
    dest_dir: &Path,
    paths: &PortablePaths,
    flatten_wrapper: bool,
) -> Result<()> {
    std::fs::create_dir_all(dest_dir)?;

    let ext = archive
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut extracted = false;
    if let Some(seven_z) = find_7zip_exe(paths) {
        let out_flag = format!("-o{}", dest_dir.display());
        let output = Command::new(&seven_z)
            .args(["x", "-y", &out_flag])
            .arg(archive)
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if output.status.success() {
            extracted = true;
        }
    }

    if !extracted && matches!(ext.as_str(), "zip" | "gz" | "tgz" | "tar" | "xz") {
        let output = Command::new("tar.exe")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if output.status.success() {
            extracted = true;
        }
    }

    if !extracted && ext == "zip" {
        let script = format!(
            "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
            archive.display().to_string().replace('\'', "''"),
            dest_dir.display().to_string().replace('\'', "''")
        );
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if output.status.success() {
            extracted = true;
        }
    }

    if !extracted {
        anyhow::bail!("无法解压归档文件（缺少 7zip 或格式不支持）: {}", archive.display());
    }

    if flatten_wrapper {
        flatten_single_subdir(dest_dir)?;
    }
    Ok(())
}

/// 如果目录中仅包含唯一一个子文件夹（无其他顶层文件），将其内容上移一层
pub fn flatten_single_subdir(dir: &Path) -> Result<()> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .collect();
    if entries.len() != 1 || !entries[0].is_dir() {
        return Ok(());
    }
    let inner = &entries[0];
    let temp_holder = dir.join("__flatten_tmp__");
    std::fs::rename(inner, &temp_holder)?;
    for item in std::fs::read_dir(&temp_holder)?.flatten() {
        let src = item.path();
        if let Some(name) = src.file_name() {
            let dst = dir.join(name);
            if dst.exists() {
                if dst.is_dir() {
                    let _ = std::fs::remove_dir_all(&dst);
                } else {
                    let _ = std::fs::remove_file(&dst);
                }
            }
            std::fs::rename(&src, &dst)?;
        }
    }
    let _ = std::fs::remove_dir_all(&temp_holder);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_content_length_from_headers;

    #[test]
    fn parses_final_content_length_across_redirects() {
        let headers = "\
HTTP/1.1 302 Found\r
Content-Length: 0\r
Location: https://cdn.example.com/Antigravity.exe\r
\r
HTTP/2 200\r
content-type: application/octet-stream\r
content-length: 242141336\r
";
        assert_eq!(parse_content_length_from_headers(headers), Some(242141336));
        assert_eq!(parse_content_length_from_headers("HTTP/2 200\r\n"), None);
    }
}

