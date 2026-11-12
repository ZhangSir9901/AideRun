// AideRun - 便携环境变量隔离注入与双模工具链 PATH 动态构造模块

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::LauncherConfig;
use crate::paths::PortablePaths;
use crate::proxy::ProxyEndpoint;

/// 构建完整隔离的进程环境变量列表（绝对基于当前目录，不硬编码盘符）
pub fn build_isolated_env(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    proxy: Option<&ProxyEndpoint>,
) -> Vec<(String, String)> {
    let mut vars = vec![
        ("USERPROFILE".into(), paths.profile_dir.display().to_string()),
        ("HOME".into(), paths.profile_dir.display().to_string()),
        ("APPDATA".into(), paths.appdata_roaming.display().to_string()),
        ("LOCALAPPDATA".into(), paths.appdata_local.display().to_string()),
        ("TEMP".into(), paths.temp_dir.display().to_string()),
        ("TMP".into(), paths.temp_dir.display().to_string()),
        ("PIP_CACHE_DIR".into(), paths.pip_cache_dir.display().to_string()),
        ("UV_CACHE_DIR".into(), paths.uv_cache_dir.display().to_string()),
        (
            "UV_PYTHON_INSTALL_DIR".into(),
            paths.portable_tools_dir.join("uv-python").display().to_string(),
        ),
        ("NPM_CONFIG_CACHE".into(), paths.npm_cache_dir.display().to_string()),
        ("npm_config_cache".into(), paths.npm_cache_dir.display().to_string()),
        (
            "NPM_CONFIG_PREFIX".into(),
            paths.profile_dir.join("npm-global").display().to_string(),
        ),
        (
            "npm_config_prefix".into(),
            paths.profile_dir.join("npm-global").display().to_string(),
        ),
        ("CARGO_HOME".into(), paths.cargo_home.display().to_string()),
        ("RUSTUP_HOME".into(), paths.rustup_home.display().to_string()),
        ("DOTNET_CLI_HOME".into(), paths.profile_dir.display().to_string()),
        (
            "GOPATH".into(),
            paths.profile_dir.join("go").display().to_string(),
        ),
        ("GOCACHE".into(), paths.go_cache_dir.display().to_string()),
        (
            "GOMODCACHE".into(),
            paths.cache_dir.join("go-mod").display().to_string(),
        ),
        (
            "XDG_CONFIG_HOME".into(),
            paths.profile_dir.join(".config").display().to_string(),
        ),
        (
            "XDG_CACHE_HOME".into(),
            paths.cache_dir.join("xdg").display().to_string(),
        ),
        (
            "GIT_CONFIG_GLOBAL".into(),
            paths.profile_dir.join(".gitconfig").display().to_string(),
        ),
    ];

    if let Some((drive, home_path)) = split_windows_drive_and_path(&paths.profile_dir) {
        vars.push(("HOMEDRIVE".into(), drive));
        vars.push(("HOMEPATH".into(), home_path));
    }

    if paths.scoop_dir.exists() {
        vars.push(("SCOOP".into(), paths.scoop_dir.display().to_string()));
    }

    if cfg.proxy.inject_env {
        if let Some(ep) = proxy {
            vars.push(("HTTP_PROXY".into(), ep.http_url()));
            vars.push(("HTTPS_PROXY".into(), ep.http_url()));
            vars.push(("ALL_PROXY".into(), ep.all_proxy_url()));
        }
    }

    let composed_path = build_portable_path(paths, cfg);
    vars.push(("PATH".into(), composed_path));
    vars
}

/// 将 Windows 绝对路径拆分为 HOMEDRIVE 与 HOMEPATH，彻底阻断旧工具按盘符逃逸至宿主机用户目录
fn split_windows_drive_and_path(path: &Path) -> Option<(String, String)> {
    let raw = path.display().to_string();
    let trimmed = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        let drive = trimmed[..2].to_string();
        let rest = trimmed[2..].replace('/', "\\");
        return Some((drive, rest));
    }
    None
}

/// 将隔离环境变量应用到子进程 Command
pub fn apply_env_to_command(cmd: &mut Command, vars: &[(String, String)]) {
    for (k, v) in vars {
        cmd.env(k, v);
    }
}

/// 将隔离环境变量应用到当前自身进程（用于内部调用 curl / 7z / innoextract 等子工具）
pub fn apply_proxy_and_tools_to_self(paths: &PortablePaths, cfg: &LauncherConfig, proxy: Option<&ProxyEndpoint>) {
    let composed_path = build_portable_path(paths, cfg);
    std::env::set_var("PATH", composed_path);
    if cfg.proxy.inject_env {
        if let Some(ep) = proxy {
            std::env::set_var("HTTP_PROXY", ep.http_url());
            std::env::set_var("HTTPS_PROXY", ep.http_url());
            std::env::set_var("ALL_PROXY", ep.all_proxy_url());
            std::env::set_var("http_proxy", ep.http_url());
            std::env::set_var("https_proxy", ep.http_url());
            std::env::set_var("all_proxy", ep.all_proxy_url());
        } else {
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                std::env::remove_var(key);
            }
        }
    }
}

/// 动态组装便携 PATH：兼顾无软链接原生工具与 Scoop 工具链（含 exFAT 直挂回退）
pub fn build_portable_path(paths: &PortablePaths, cfg: &LauncherConfig) -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if cfg.toolchain.prefer_native_portable {
        collect_native_portable_bins(paths, cfg, &mut candidates);
        collect_scoop_bins(paths, &mut candidates);
    } else {
        collect_scoop_bins(paths, &mut candidates);
        collect_native_portable_bins(paths, cfg, &mut candidates);
    }

    let npm_global = paths.profile_dir.join("npm-global");
    candidates.push(npm_global.join("bin"));
    candidates.push(npm_global);
    candidates.push(paths.tools_dir.clone());
    candidates.push(paths.bin_dir.clone());

    let mut seen: HashSet<String> = HashSet::new();
    let mut final_parts: Vec<String> = Vec::new();

    for dir in candidates {
        if dir.is_dir() {
            let s = dir.display().to_string();
            let key = s.to_ascii_lowercase();
            if seen.insert(key) {
                final_parts.push(s);
            }
        }
    }

    if let Ok(sys_path) = std::env::var("PATH") {
        for raw in sys_path.split(';') {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let key = trimmed.to_ascii_lowercase();
            if seen.insert(key) {
                final_parts.push(trimmed.to_string());
            }
        }
    }

    final_parts.join(";")
}

fn collect_native_portable_bins(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    out: &mut Vec<PathBuf>,
) {
    if !paths.portable_tools_dir.is_dir() {
        return;
    }
    for custom in &cfg.toolchain.custom_components {
        let base = paths.portable_tools_dir.join(&custom.name);
        if custom.bin_subpaths.is_empty() {
            out.push(base);
        } else {
            for sub in &custom.bin_subpaths {
                out.push(base.join(sub));
            }
        }
    }

    // 自动探测常见原生免安装目录布局（uv、mingit、nodejs、python、7zip、innoextract 等）
    if let Ok(entries) = std::fs::read_dir(&paths.portable_tools_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            out.push(p.clone());
            for sub in ["bin", "cmd", "Scripts", "mingw64/bin", "usr/bin"] {
                let candidate = p.join(sub);
                if candidate.is_dir() {
                    out.push(candidate);
                }
            }
        }
    }
}

fn collect_scoop_bins(paths: &PortablePaths, out: &mut Vec<PathBuf>) {
    if !paths.scoop_dir.is_dir() {
        return;
    }
    out.push(paths.scoop_dir.join("shims"));

    let apps = paths.scoop_dir.join("apps");
    let specs: &[(&str, &[&str])] = &[
        ("nodejs-lts", &["", "bin"]),
        ("python", &["", "Scripts"]),
        ("git", &["cmd", "mingw64/bin"]),
        ("rust", &["bin"]),
        ("7zip", &[""]),
        ("go", &["bin"]),
    ];

    for (app, subdirs) in specs {
        let app_dir = apps.join(app);
        if !app_dir.is_dir() {
            continue;
        }
        // 优先尝试 current，若在 FAT32/exFAT 上无 Junction，则直接回退到真实版本号目录
        let base = resolve_scoop_app_root(&app_dir);
        if let Some(root) = base {
            for sub in *subdirs {
                if sub.is_empty() {
                    out.push(root.clone());
                } else {
                    out.push(root.join(sub));
                }
            }
        }
    }
}

/// 获取 Scoop 应用的有效根路径：优先 current，失效或非 NTFS 时自动回退到具体版本号文件夹
pub fn resolve_scoop_app_root(app_dir: &Path) -> Option<PathBuf> {
    let current = app_dir.join("current");
    if current.is_dir() && std::fs::read_dir(&current).is_ok() {
        return Some(current);
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

#[cfg(test)]
mod tests {
    use super::split_windows_drive_and_path;
    use std::path::Path;

    #[test]
    fn splits_windows_drive_and_home_path_cleanly() {
        assert_eq!(
            split_windows_drive_and_path(Path::new(r"D:\Antigravity IDE\data\profile")),
            Some(("D:".to_string(), r"\Antigravity IDE\data\profile".to_string()))
        );
        assert_eq!(
            split_windows_drive_and_path(Path::new(r"\\?\E:\Work\data\profile")),
            Some(("E:".to_string(), r"\Work\data\profile".to_string()))
        );
    }
}

