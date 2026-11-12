// AideRun - 便携目录拓扑解析与相对路径统一管理模块

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 便携根目录及全套隔离路径集合
#[derive(Debug, Clone)]
pub struct PortablePaths {
    pub root: PathBuf,
    pub self_exe: PathBuf,
    pub ide_exe: PathBuf,
    pub resources_dir: PathBuf,
    pub data_dir: PathBuf,
    pub user_data_dir: PathBuf,
    pub extensions_dir: PathBuf,
    pub profile_dir: PathBuf,
    pub appdata_roaming: PathBuf,
    pub appdata_local: PathBuf,
    pub gemini_dir: PathBuf,
    pub cargo_home: PathBuf,
    pub rustup_home: PathBuf,
    pub cache_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub crash_dumps_dir: PathBuf,
    pub pip_cache_dir: PathBuf,
    pub npm_cache_dir: PathBuf,
    pub uv_cache_dir: PathBuf,
    pub go_cache_dir: PathBuf,
    pub projects_dir: PathBuf,
    pub tools_dir: PathBuf,
    pub scoop_dir: PathBuf,
    pub portable_tools_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub proxy_config: PathBuf,
    pub launcher_config: PathBuf,
    pub root_marker: PathBuf,
    pub state_vscdb: PathBuf,
    pub argv_json: PathBuf,
    pub settings_json: PathBuf,
    pub upgrade_backup_dir: PathBuf,
}

impl PortablePaths {
    /// 从当前可执行文件所在目录自动定位全部便携路径
    pub fn discover() -> Result<Self> {
        let exe = std::env::current_exe().context("无法获取当前启动器可执行文件路径")?;
        let canon_exe = dunce::canonicalize(&exe).unwrap_or(exe);
        let root = canon_exe
            .parent()
            .context("无法定位启动器所在根目录")?
            .to_path_buf();
        Ok(Self::from_root(root, canon_exe))
    }

    /// 基于指定根目录构造路径拓扑（便于测试与沙盒演练）
    pub fn from_root(root: PathBuf, self_exe: PathBuf) -> Self {
        let data_dir = root.join("data");
        let user_data_dir = data_dir.join("user-data");
        let extensions_dir = data_dir.join("extensions");
        let profile_dir = data_dir.join("profile");
        let appdata_dir = profile_dir.join("AppData");
        let cache_dir = data_dir.join("cache");
        let tools_dir = root.join("tools");

        Self {
            ide_exe: root.join("Antigravity IDE.exe"),
            resources_dir: root.join("resources"),
            appdata_roaming: appdata_dir.join("Roaming"),
            appdata_local: appdata_dir.join("Local"),
            gemini_dir: profile_dir.join(".gemini"),
            cargo_home: profile_dir.join(".cargo"),
            rustup_home: profile_dir.join(".rustup"),
            temp_dir: cache_dir.join("temp"),
            logs_dir: cache_dir.join("logs"),
            crash_dumps_dir: cache_dir.join("crash-dumps"),
            pip_cache_dir: cache_dir.join("pip"),
            npm_cache_dir: cache_dir.join("npm"),
            uv_cache_dir: cache_dir.join("uv"),
            go_cache_dir: cache_dir.join("go-build"),
            projects_dir: root.join("projects"),
            scoop_dir: tools_dir.join("scoop"),
            portable_tools_dir: tools_dir.join("portable"),
            bin_dir: root.join("bin"),
            proxy_config: root.join("config.json"),
            launcher_config: data_dir.join("launcher.toml"),
            root_marker: data_dir.join(".portable_root"),
            state_vscdb: user_data_dir
                .join("User")
                .join("globalStorage")
                .join("state.vscdb"),
            argv_json: data_dir.join("argv.json"),
            settings_json: user_data_dir.join("User").join("settings.json"),
            upgrade_backup_dir: data_dir.join("_upgrade_backup"),
            data_dir,
            user_data_dir,
            extensions_dir,
            profile_dir,
            cache_dir,
            tools_dir,
            self_exe,
            root,
        }
    }

    /// 判断便携运行目录是否尚未部署（如仅有 AideRun.exe 与 Antigravity IDE.exe）
    pub fn needs_portable_bootstrap(&self) -> bool {
        !self.data_dir.exists()
            || !self.user_data_dir.exists()
            || !self.profile_dir.exists()
            || !self.tools_dir.exists()
    }

    /// 创建全部基础便携目录骨架
    pub fn ensure_skeleton(&self) -> Result<()> {
        let dirs = [
            &self.data_dir,
            &self.user_data_dir,
            &self.extensions_dir,
            &self.profile_dir,
            &self.appdata_roaming,
            &self.appdata_local,
            &self.gemini_dir,
            &self.cargo_home,
            &self.rustup_home,
            &self.cache_dir,
            &self.temp_dir,
            &self.logs_dir,
            &self.crash_dumps_dir,
            &self.pip_cache_dir,
            &self.npm_cache_dir,
            &self.uv_cache_dir,
            &self.go_cache_dir,
            &self.projects_dir,
            &self.tools_dir,
            &self.portable_tools_dir,
        ];
        for dir in dirs {
            if !dir.exists() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("创建目录失败: {}", dir.display()))?;
            }
        }
        let user_dir = self.user_data_dir.join("User");
        for extra in [user_dir, self.profile_dir.join("npm-global")] {
            if !extra.exists() {
                std::fs::create_dir_all(&extra)?;
            }
        }
        Ok(())
    }

    /// 获取相对于根目录的可读显示路径
    pub fn rel_display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_detection_triggers_when_only_exe_files_exist() {
        let tmp = std::env::temp_dir().join(format!("aiderun_paths_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("AideRun.exe"), b"").unwrap();
        std::fs::write(tmp.join("Antigravity IDE.exe"), b"").unwrap();

        let paths = PortablePaths::from_root(tmp.clone(), tmp.join("AideRun.exe"));
        assert!(paths.needs_portable_bootstrap());

        paths.ensure_skeleton().unwrap();
        assert!(!paths.needs_portable_bootstrap());
        assert!(paths.user_data_dir.is_dir());
        assert!(paths.portable_tools_dir.is_dir());
        assert!(paths.projects_dir.is_dir());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

