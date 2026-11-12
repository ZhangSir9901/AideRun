// AideRun - 启动器配置模型与 data/launcher.toml 读写模块

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 自定义便携工具组件定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomComponent {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub bin_subpaths: Vec<String>,
    #[serde(default)]
    pub description: String,
}

/// 启动行为配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchConfig {
    pub locale: String,
    pub default_workspace: String,
    pub disable_gpu_disk_cache: bool,
    pub create_desktop_shortcut: bool,
    pub auto_clean_on_exit: bool,
    pub smart_clean: bool,
    pub extra_args: Vec<String>,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            locale: "zh-cn".to_string(),
            default_workspace: "projects".to_string(),
            disable_gpu_disk_cache: true,
            create_desktop_shortcut: false,
            auto_clean_on_exit: false,
            smart_clean: false,
            extra_args: Vec::new(),
        }
    }
}

/// 代理配置策略
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyPolicyConfig {
    pub sync_system_proxy: bool,
    pub inject_env: bool,
    pub lock_manual: bool,
    pub manual_proxy: Option<String>,
}

impl Default for ProxyPolicyConfig {
    fn default() -> Self {
        Self {
            sync_system_proxy: true,
            inject_env: true,
            lock_manual: false,
            manual_proxy: None,
        }
    }
}

/// 工具链管理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolchainConfig {
    pub auto_heal_scoop: bool,
    pub prefer_native_portable: bool,
    pub custom_components: Vec<CustomComponent>,
}

impl Default for ToolchainConfig {
    fn default() -> Self {
        Self {
            auto_heal_scoop: false,
            prefer_native_portable: true,
            custom_components: Vec::new(),
        }
    }
}

/// 垃圾清理自定义规则
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CleanupConfig {
    pub extra_clean_paths: Vec<String>,
    pub preserve_paths: Vec<String>,
}

/// 升级与版本检测配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpgradeConfig {
    pub releases_api_url: String,
    pub download_page_url: String,
    pub releases_page_url: String,
    pub auto_check_interval_hours: u64,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            releases_api_url:
                "https://antigravity-ide-auto-updater-974169037036.us-central1.run.app/releases"
                    .to_string(),
            download_page_url: "https://antigravity.google/download".to_string(),
            releases_page_url: "https://antigravity.google/releases".to_string(),
            auto_check_interval_hours: 24,
        }
    }
}

/// 启动器顶层配置对象（存放于 data/launcher.toml，缺失时使用全套安全默认值）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LauncherConfig {
    pub launch: LaunchConfig,
    pub proxy: ProxyPolicyConfig,
    pub toolchain: ToolchainConfig,
    pub cleanup: CleanupConfig,
    pub upgrade: UpgradeConfig,
}

impl LauncherConfig {
    /// 读取配置文件；文件不存在时直接返回内置默认值，不主动生成多余文件
    pub fn load_or_default(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str::<Self>(raw.trim_start_matches('\u{FEFF}')).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 保存配置至 data/launcher.toml
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self).context("序列化 launcher.toml 失败")?;
        let content = format!(
            "# AideRun - 便携版启动器自定义配置文件（所有字段均可删去以使用内置默认值）\n{body}"
        );
        std::fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))
    }
}

