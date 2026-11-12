// AideRun - 启动前网络代理自检与便携环境一键部署独立子窗口控制器（与主界面统一 1120x700）

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tao::dpi::LogicalSize;
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Icon as WindowIcon, Window, WindowBuilder};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::bootstrap::deploy_portable_ide_from_official;
use crate::config::LauncherConfig;
use crate::gui::{
    center_window_on_monitor, emit_event_json, make_brand_logo_data_uri, AppEvent,
    APP_WINDOW_HEIGHT, APP_WINDOW_WIDTH,
};
use crate::paths::PortablePaths;
use crate::proxy::{
    auto_detect_google_proxy, check_proxy_reachable_timeout, parse_proxy_candidates,
    parse_proxy_string, probe_google_access, read_windows_system_proxy, GoogleProbeReport,
    ProxyEndpoint,
};
use crate::self_update::open_github_repository;

pub const PROXY_WINDOW_TITLE: &str = "AideRun - 网络代理配置";
pub const DEPLOY_WINDOW_TITLE: &str = "AideRun - Antigravity IDE 便携部署";

const PREBOOT_HTML_TEMPLATE: &str = include_str!("../assets/preboot.html");

/// 独立前置子窗口当前所处阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrebootStage {
    Proxy,
    Deploy,
}

/// 前置子窗口处理前端 IPC 后的控制流反馈
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrebootIpcAction {
    Continue,
    QuitApp,
}

/// 启动前置独立水晶玻璃子窗口会话（声明 webview 在 window 之前，保证 Drop 时先释放 WebView2 再销毁宿主窗口）
pub struct PrebootSession {
    pub stage: PrebootStage,
    webview: WebView,
    pub window: Window,
    paths: PortablePaths,
    cfg: LauncherConfig,
    save_manual: bool,
    proxy: EventLoopProxy<AppEvent>,
    busy_proxy: Arc<AtomicBool>,
    deploy_busy: Arc<AtomicBool>,
}

impl PrebootSession {
    /// 构建「Google 代理检测与配置」子窗口（与主界面统一 1120x700）
    pub fn open_proxy(
        el_target: &EventLoopWindowTarget<AppEvent>,
        web_ctx: &mut WebContext,
        win_icon: &WindowIcon,
        el_proxy: &EventLoopProxy<AppEvent>,
        paths: PortablePaths,
        cfg: LauncherConfig,
        probe: &GoogleProbeReport,
    ) -> Result<Self> {
        let suggested = cfg
            .proxy
            .manual_proxy
            .as_deref()
            .and_then(parse_proxy_string)
            .or_else(read_windows_system_proxy)
            .map(|ep| ep.all_proxy_url());

        let listening_ports: Vec<Value> = probe
            .open_local_ports
            .iter()
            .map(|hit| {
                json!({
                    "scheme": hit.endpoint.proxy_type,
                    "port": hit.endpoint.port,
                    "label": hit.software,
                })
            })
            .collect();

        let init_payload = json!({
            "mode": "proxy",
            "suggested_proxy": suggested,
            "running_clients": probe.running_clients,
            "listening_ports": listening_ports,
            "probe_detail": probe.failure_reason,
        });

        Self::build_stage(
            PrebootStage::Proxy,
            PROXY_WINDOW_TITLE,
            (APP_WINDOW_WIDTH, APP_WINDOW_HEIGHT),
            &init_payload,
            el_target,
            web_ctx,
            win_icon,
            el_proxy,
            paths,
            cfg,
            false,
        )
    }

    /// 构建「便携 Antigravity IDE 环境确认与一键下载部署」子窗口（与主界面统一 1120x700）
    pub fn open_deploy(
        el_target: &EventLoopWindowTarget<AppEvent>,
        web_ctx: &mut WebContext,
        win_icon: &WindowIcon,
        el_proxy: &EventLoopProxy<AppEvent>,
        paths: PortablePaths,
        cfg: LauncherConfig,
        save_manual: bool,
    ) -> Result<Self> {
        let init_payload = json!({
            "mode": "deploy",
            "root_dir": paths.root.display().to_string(),
        });

        Self::build_stage(
            PrebootStage::Deploy,
            DEPLOY_WINDOW_TITLE,
            (APP_WINDOW_WIDTH, APP_WINDOW_HEIGHT),
            &init_payload,
            el_target,
            web_ctx,
            win_icon,
            el_proxy,
            paths,
            cfg,
            save_manual,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_stage(
        stage: PrebootStage,
        title: &str,
        size: (f64, f64),
        init_payload: &Value,
        el_target: &EventLoopWindowTarget<AppEvent>,
        web_ctx: &mut WebContext,
        win_icon: &WindowIcon,
        el_proxy: &EventLoopProxy<AppEvent>,
        paths: PortablePaths,
        cfg: LauncherConfig,
        save_manual: bool,
    ) -> Result<Self> {
        let window = WindowBuilder::new()
            .with_title(title)
            .with_inner_size(LogicalSize::new(size.0, size.1))
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_undecorated_shadow(false)
            .with_window_icon(Some(win_icon.clone()))
            .with_taskbar_icon(Some(win_icon.clone()))
            .build(el_target)
            .with_context(|| format!("创建前置引导子窗口 [{title}] 失败"))?;

        center_window_on_monitor(&window);

        let html = render_preboot_html(init_payload);
        let ipc_proxy = el_proxy.clone();
        let webview = WebViewBuilder::new_with_web_context(web_ctx)
            .with_transparent(true)
            .with_devtools(cfg!(debug_assertions))
            .with_html(html)
            .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
            .with_ipc_handler(move |req| {
                let _ = ipc_proxy.send_event(AppEvent::Ipc(req.into_body()));
            })
            .build(&window)
            .context("初始化前置子窗口 WebView2 失败（请确认系统已安装 Edge WebView2 Runtime）")?;

        Ok(Self {
            stage,
            webview,
            window,
            paths,
            cfg,
            save_manual,
            proxy: el_proxy.clone(),
            busy_proxy: Arc::new(AtomicBool::new(false)),
            deploy_busy: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 判断当前是否处于官网下载解压部署锁定期（防止误关闭造成半残目录）
    pub fn is_deploying(&self) -> bool {
        self.deploy_busy.load(AtomicOrdering::SeqCst)
    }

    /// 向当前前置子窗口执行脚本回调
    pub fn eval_js(&self, js: &str) {
        let _ = self.webview.evaluate_script(js);
    }

    /// 分发来自前置子窗口的 IPC 操作
    pub fn handle_ipc(&self, action: &str, payload: Value) -> PrebootIpcAction {
        match action {
            "drag_window" => {
                let _ = self.window.drag_window();
                PrebootIpcAction::Continue
            }
            "open_github_repo" => {
                let _ = open_github_repository();
                PrebootIpcAction::Continue
            }
            "quit_app" => {
                if self.is_deploying() {
                    PrebootIpcAction::Continue
                } else {
                    PrebootIpcAction::QuitApp
                }
            }
            "test_manual_proxy" if self.stage == PrebootStage::Proxy => {
                self.spawn_test_manual_proxy(payload);
                PrebootIpcAction::Continue
            }
            "recheck_auto_proxy" if self.stage == PrebootStage::Proxy => {
                self.spawn_recheck_auto_proxy();
                PrebootIpcAction::Continue
            }
            "confirm_deploy" if self.stage == PrebootStage::Deploy => {
                self.spawn_official_deploy();
                PrebootIpcAction::Continue
            }
            _ => PrebootIpcAction::Continue,
        }
    }

    fn spawn_test_manual_proxy(&self, payload: Value) {
        if self
            .busy_proxy
            .compare_exchange(false, true, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
            .is_err()
        {
            return;
        }
        let raw = payload
            .get("proxy")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let el_proxy = self.proxy.clone();
        let busy_flag = self.busy_proxy.clone();

        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let candidates = parse_proxy_candidates(&raw);
                if candidates.is_empty() {
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "proxy_result",
                            "ok": false,
                            "message": "无法识别代理信息。支持 http://ip:port、socks5://user:pass@ip:port、ip:port:用户名:密码 或复制完整服务器配置文本。",
                        }),
                    );
                    return;
                }

                // 若 TCP 端口本身均无法建立连接，直接快速提示，避免无效多协议回退等待
                if !check_proxy_reachable_timeout(&candidates[0], Duration::from_millis(900)) {
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "proxy_result",
                            "ok": false,
                            "message": format!("无法连接代理 {}:{}（未开启或网络不可达）", candidates[0].host, candidates[0].port),
                        }),
                    );
                    return;
                }

                let mut last_err = String::new();
                for ep in &candidates {
                    match probe_google_access(Some(ep)) {
                        Ok(()) => {
                            emit_event_json(
                                &el_proxy,
                                json!({
                                    "kind": "proxy_result",
                                    "ok": true,
                                    "resolved_proxy": ep.all_proxy_url(),
                                    "message": format!(
                                        "✓ 已智能识别 {} ({}) 并成功连通 Google，正在进入下一步...",
                                        ep.normalized_scheme().to_uppercase(),
                                        ep.masked_url()
                                    ),
                                }),
                            );
                            std::thread::sleep(Duration::from_millis(350));
                            let _ = el_proxy.send_event(AppEvent::PrebootProxyPassed {
                                endpoint: Some(ep.clone()),
                                save_manual: true,
                            });
                            return;
                        }
                        Err(err) => {
                            last_err = format!(
                                "{} ({})：{err}",
                                ep.normalized_scheme().to_uppercase(),
                                ep.masked_url()
                            );
                        }
                    }
                }

                emit_event_json(
                    &el_proxy,
                    json!({
                        "kind": "proxy_result",
                        "ok": false,
                        "message": format!("Google 连通验证失败 — {last_err}"),
                    }),
                );
            }));

            busy_flag.store(false, AtomicOrdering::SeqCst);
            if res.is_err() {
                emit_event_json(
                    &el_proxy,
                    json!({
                        "kind": "proxy_result",
                        "ok": false,
                        "message": "检测代理过程遇到内部异常，请重试。",
                    }),
                );
            }
        });
    }

    fn spawn_recheck_auto_proxy(&self) {
        if self
            .busy_proxy
            .compare_exchange(false, true, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
            .is_err()
        {
            return;
        }
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let el_proxy = self.proxy.clone();
        let busy_flag = self.busy_proxy.clone();

        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let report = auto_detect_google_proxy(&paths, &cfg);
                if report.reachable {
                    let detected_url = report.endpoint.as_ref().map(|ep| ep.all_proxy_url());
                    let msg = if let Some(ep) = &report.endpoint {
                        format!(
                            "✓ 自动检测到可用代理 {}（{}），已自动填入，正在进入下一步...",
                            ep.masked_url(),
                            report.source_label
                        )
                    } else {
                        format!(
                            "✓ 检测到当前系统网络可直接连通 Google（{}），正在进入下一步...",
                            report.source_label
                        )
                    };
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "proxy_result",
                            "ok": true,
                            "detected_proxy": detected_url,
                            "is_direct": report.endpoint.is_none(),
                            "message": msg,
                        }),
                    );
                    std::thread::sleep(Duration::from_millis(450));
                    let _ = el_proxy.send_event(AppEvent::PrebootProxyPassed {
                        endpoint: report.endpoint,
                        save_manual: false,
                    });
                } else {
                    let suggested = read_windows_system_proxy()
                        .or_else(|| report.open_local_ports.first().map(|hit| hit.endpoint.clone()))
                        .map(|ep| ep.all_proxy_url());
                    let msg = if suggested.is_some() {
                        format!(
                            "已自动填入探测到的本机代理候选，但暂未连通 Google（请检查节点或粘贴外部代理）：{}",
                            report.failure_reason
                        )
                    } else {
                        format!(
                            "未检测到可用本地代理且直连失败（{}），请在上方粘贴外部代理信息",
                            report.failure_reason
                        )
                    };
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "proxy_result",
                            "ok": false,
                            "suggested_proxy": suggested,
                            "probe_detail": report.failure_reason,
                            "message": msg,
                        }),
                    );
                }
            }));

            busy_flag.store(false, AtomicOrdering::SeqCst);
            if res.is_err() {
                emit_event_json(
                    &el_proxy,
                    json!({
                        "kind": "proxy_result",
                        "ok": false,
                        "message": "自动重新检测遇到内部异常，请粘贴代理地址后测试。",
                    }),
                );
            }
        });
    }

    fn spawn_official_deploy(&self) {
        if self
            .deploy_busy
            .compare_exchange(false, true, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
            .is_err()
        {
            return;
        }

        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let save_manual = self.save_manual;
        let el_proxy = self.proxy.clone();
        let busy_flag = self.deploy_busy.clone();

        std::thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                deploy_portable_ide_from_official(&paths, &cfg, save_manual, |prog| {
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "deploy_progress",
                            "percent": prog.percent,
                            "stage": prog.stage,
                            "detail": prog.detail,
                            "log_line": prog.log_line,
                        }),
                    );
                })
            }));

            match res {
                Ok(Ok(_ver)) => {
                    std::thread::sleep(Duration::from_millis(450));
                    let _ = el_proxy.send_event(AppEvent::PrebootDeployCompleted);
                }
                Ok(Err(err)) => {
                    busy_flag.store(false, AtomicOrdering::SeqCst);
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "deploy_failed",
                            "error": format!("{err:#}"),
                        }),
                    );
                }
                Err(_) => {
                    busy_flag.store(false, AtomicOrdering::SeqCst);
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "deploy_failed",
                            "error": "在线部署线程遇到未捕获异常，请查看 data/cache/logs/panic.log",
                        }),
                    );
                }
            }
        });
    }
}

/// 将经由前置界面确认的有效代理同步应用到内存配置，并在需要时更新 config.json
pub fn apply_confirmed_proxy_to_cfg(
    paths: &PortablePaths,
    cfg: &mut LauncherConfig,
    endpoint: Option<&ProxyEndpoint>,
    save_manual: bool,
) -> Option<ProxyEndpoint> {
    if save_manual {
        if let Some(ep) = endpoint {
            cfg.proxy.manual_proxy = Some(ep.all_proxy_url());
            cfg.proxy.lock_manual = true;
            if paths.launcher_config.parent().is_some_and(|p| p.is_dir()) {
                let _ = cfg.save(&paths.launcher_config);
            }
        }
    }
    let fallback = crate::proxy::resolve_and_sync_proxy(&paths.proxy_config, &cfg.proxy);
    let effective = endpoint.cloned().or(fallback);
    crate::env::apply_proxy_and_tools_to_self(paths, cfg, effective.as_ref());
    effective
}

fn render_preboot_html(init_payload: &Value) -> String {
    let safe_json = init_payload
        .to_string()
        .replace("</", "<\\/")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    PREBOOT_HTML_TEMPLATE
        .replace("{{BRAND_LOGO_DATA_URI}}", &make_brand_logo_data_uri())
        .replace("{{PREBOOT_INIT_JSON}}", &safe_json)
}

#[cfg(test)]
mod tests {
    use super::PREBOOT_HTML_TEMPLATE;

    #[test]
    fn preboot_css_uses_rgba_not_wpf_argb_hex_and_preserves_footer_buttons() {
        for bad_hex in ["#0DFFFFFF", "#35FFFFFF", "#18FFFFFF", "#40FFFFFF"] {
            assert!(
                !PREBOOT_HTML_TEMPLATE.contains(bad_hex),
                "WebView CSS 中不得出现 #AARRGGBB 格式色值 {bad_hex}（Chromium 会误判为青色 #RRGGBBAA）"
            );
        }
        assert!(PREBOOT_HTML_TEMPLATE.contains("测试连接并继续"));
        assert!(PREBOOT_HTML_TEMPLATE.contains("是，立即下载部署"));
        assert!(PREBOOT_HTML_TEMPLATE.contains("proxyUrlInput"));
        assert!(!PREBOOT_HTML_TEMPLATE.contains("proxyScheme"));
        assert!(PREBOOT_HTML_TEMPLATE.contains("btnGithubTop"));
        assert!(PREBOOT_HTML_TEMPLATE.contains("btnGithubCorner"));
        assert!(PREBOOT_HTML_TEMPLATE.contains("open_github_repo"));
    }
}
