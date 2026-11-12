// AideRun - 启动前网络代理自检与便携环境一键部署独立子窗口控制器

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Icon as WindowIcon, Window, WindowBuilder};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::bootstrap::deploy_portable_ide_from_official;
use crate::config::LauncherConfig;
use crate::gui::{emit_event_json, make_brand_logo_data_uri, AppEvent};
use crate::paths::PortablePaths;
use crate::proxy::{
    auto_detect_google_proxy, parse_proxy_string, probe_google_access, read_windows_system_proxy,
    GoogleProbeReport, ProxyEndpoint,
};

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
    /// 构建「Google 代理检测与配置」子窗口（480x410）
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
            (480.0, 410.0),
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

    /// 构建「便携 Antigravity IDE 环境确认与一键下载部署」子窗口（500x360）
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
            (500.0, 360.0),
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
                let Some(ep) = parse_proxy_string(&raw) else {
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "proxy_result",
                            "ok": false,
                            "message": "代理地址格式无效，请输入如 http://127.0.0.1:7890 或 socks5://127.0.0.1:10808",
                        }),
                    );
                    return;
                };

                match probe_google_access(Some(&ep)) {
                    Ok(()) => {
                        emit_event_json(
                            &el_proxy,
                            json!({
                                "kind": "proxy_result",
                                "ok": true,
                                "message": format!("✓ 通过 {} 成功连通 Google，正在进入下一步...", ep.all_proxy_url()),
                            }),
                        );
                        std::thread::sleep(Duration::from_millis(300));
                        let _ = el_proxy.send_event(AppEvent::PrebootProxyPassed {
                            endpoint: Some(ep),
                            save_manual: true,
                        });
                    }
                    Err(err) => {
                        emit_event_json(
                            &el_proxy,
                            json!({
                                "kind": "proxy_result",
                                "ok": false,
                                "message": format!("Google 连通验证失败：{err}"),
                            }),
                        );
                    }
                }
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
                    let via = if let Some(ep) = &report.endpoint {
                        format!("{} ({})", report.source_label, ep.all_proxy_url())
                    } else {
                        report.source_label.clone()
                    };
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "proxy_result",
                            "ok": true,
                            "message": format!("✓ 自动检测通过：{via}，正在进入下一步..."),
                        }),
                    );
                    std::thread::sleep(Duration::from_millis(300));
                    let _ = el_proxy.send_event(AppEvent::PrebootProxyPassed {
                        endpoint: report.endpoint,
                        save_manual: false,
                    });
                } else {
                    emit_event_json(
                        &el_proxy,
                        json!({
                            "kind": "proxy_result",
                            "ok": false,
                            "message": format!("自动检测仍无法访问 Google：{}", report.failure_reason),
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
                        "message": "自动重新检测遇到内部异常，请手动填写代理后测试。",
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

fn center_window_on_monitor(window: &Window) {
    let Some(mon) = window
        .current_monitor()
        .or_else(|| window.primary_monitor())
    else {
        return;
    };
    let mon_pos = mon.position();
    let mon_size = mon.size();
    let win_size = window.outer_size();
    let x = mon_pos.x + (mon_size.width as i32 - win_size.width as i32) / 2;
    let y = mon_pos.y + (mon_size.height as i32 - win_size.height as i32) / 2;
    window.set_outer_position(PhysicalPosition::new(x, y));
}
