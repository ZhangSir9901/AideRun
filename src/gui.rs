// AideRun - 860x560 水晶玻璃拟态 WebView2 控制台、启动前置子窗口编排、防闪退护盾与系统托盘事件引擎

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Icon as WindowIcon, Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon as TrayIconImg, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::bootstrap::{ensure_portable_ready, inspect_portable_env_status, PortableEnvStatus};
use crate::clean::{
    format_bytes, measure_path_size, run_clean_with_opts, run_smart_daily_clean_if_due,
    scan_clean_breakdown, CleanBreakdownItem, CleanLevel,
};
use crate::config::LauncherConfig;
use crate::console::{show_error_dialog, to_wide_null};
use crate::doctor::collect_doctor_report;
use crate::env::apply_proxy_and_tools_to_self;
use crate::launch::{
    create_desktop_shortcut, launch_ide_isolated_instance, launch_ide_new_window,
    list_isolated_instances, list_project_folders, pick_folder_dialog, pick_installer_file_dialog,
    spawn_portable_shell_window,
};
use crate::paths::PortablePaths;
use crate::preboot_gui::{
    apply_confirmed_proxy_to_cfg, PrebootIpcAction, PrebootSession, DEPLOY_WINDOW_TITLE,
    PROXY_WINDOW_TITLE,
};
use crate::process::{find_active_processes_in_root, find_blocking_ide_processes};
use crate::proxy::{
    auto_detect_google_proxy, check_proxy_reachable, read_config_json_proxy,
    read_windows_system_proxy, ProxyEndpoint,
};
use crate::sanitize::run_sanitize;
use crate::scoop_heal::is_ntfs_volume;
use crate::self_update::{
    check_self_update, download_and_swap_self, open_github_repository,
    spawn_new_self_after_update, SelfUpdateCheckResult,
};
use crate::toolbox::{
    add_custom_component, collect_component_statuses, install_portable_tool, remove_portable_tool,
    ComponentStatusItem,
};
use crate::upgrade::{
    check_latest_release, compare_semver, execute_upgrade, is_inno_setup_installer,
    read_local_versions, rollback_upgrade,
};

const PANEL_TITLE: &str = "AideRun - Antigravity IDE 便携控制台";
const PANEL_HTML_TEMPLATE: &str = include_str!("../assets/panel.html");
const BRAND_LOGO_SVG: &str = include_str!("../assets/logo.svg");
const WIN_ICON_RGBA: &[u8] = include_bytes!("../assets/win_64.rgba");
const TRAY_ICON_RGBA: &[u8] = include_bytes!("../assets/tray_32.rgba");

#[derive(Debug)]
pub(crate) enum AppEvent {
    Ipc(String),
    TrayClick,
    MenuAction(String),
    EvalJs(String),
    HideToTray,
    PrebootProxyPassed {
        endpoint: Option<ProxyEndpoint>,
        save_manual: bool,
    },
    PrebootDeployCompleted,
    SelfUpdateRestart,
}

#[derive(Debug, Deserialize)]
struct IpcMessage {
    action: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Serialize)]
struct ActiveProcItem {
    pid: u32,
    name: String,
}

#[derive(Debug, Serialize)]
struct ProjectItem {
    name: String,
    path: String,
}

#[derive(Debug, Clone, Serialize)]
struct CachedUpdateCheck {
    ok: bool,
    local_ver: Option<String>,
    remote_ver: Option<String>,
    download_url: Option<String>,
    source_label: Option<String>,
    has_update: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DashboardState {
    aiderun_version: &'static str,
    root_dir: String,
    ide_installed: bool,
    ide_needs_unpack: bool,
    ide_version: Option<String>,
    needs_bootstrap: bool,
    proxy_text: String,
    ntfs: bool,
    active_procs: Vec<ActiveProcItem>,
    auto_clean_on_exit: bool,
    smart_clean: bool,
    projects: Vec<ProjectItem>,
    instances: Vec<String>,
    data_total_fmt: String,
    daily_clean_fmt: String,
    pack_clean_fmt: String,
    junk_breakdown: Vec<CleanBreakdownItem>,
    innounp_ready: bool,
    components: Vec<ComponentStatusItem>,
}

#[derive(Clone)]
struct SharedApp {
    paths: PortablePaths,
    cfg: Arc<Mutex<LauncherConfig>>,
    proxy: EventLoopProxy<AppEvent>,
    update_cache: Arc<Mutex<Option<CachedUpdateCheck>>>,
    self_update_cache: Arc<Mutex<Option<SelfUpdateCheckResult>>>,
    remote_tool_vers: Arc<Mutex<HashMap<String, String>>>,
    busy_clean: Arc<AtomicBool>,
    busy_upgrade: Arc<AtomicBool>,
    busy_tools: Arc<AtomicBool>,
    busy_update: Arc<AtomicBool>,
    busy_self_update: Arc<AtomicBool>,
    smart_boot_checked: Arc<AtomicBool>,
    self_update_checked: Arc<AtomicBool>,
}

struct BusyGuard(Arc<AtomicBool>);

impl BusyGuard {
    fn try_acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        if flag
            .compare_exchange(false, true, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
            .is_ok()
        {
            Some(Self(flag.clone()))
        } else {
            None
        }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, AtomicOrdering::SeqCst);
    }
}

fn read_cfg(app: &SharedApp) -> LauncherConfig {
    app.cfg
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn update_cfg<F: FnOnce(&mut LauncherConfig)>(app: &SharedApp, f: F) -> LauncherConfig {
    let mut guard = app
        .cfg
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard);
    guard.clone()
}

/// 若主面板或任意前置子窗口已在运行，则直接唤醒置前，避免重复开启多份实例
fn try_focus_existing_panel() -> bool {
    for title in [PANEL_TITLE, PROXY_WINDOW_TITLE, DEPLOY_WINDOW_TITLE] {
        let title_w = to_wide_null(title);
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), title_w.as_ptr());
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_SHOW);
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
                return true;
            }
        }
    }
    false
}

/// 主控制台运行时对象集合（webview 声明先于 window / _web_context 以确保按序安全释放）
struct MainRuntime {
    webview: WebView,
    window: Window,
    _web_context: WebContext,
    tray_holder: Option<tray_icon::TrayIcon>,
    shutdown_flag: Arc<AtomicBool>,
    shared: SharedApp,
}

enum ActiveUi {
    Preboot(PrebootSession),
    Main(MainRuntime),
}

fn cleanup_preboot_resources(
    active_ui: &mut Option<ActiveUi>,
    preboot_web_ctx: &mut Option<WebContext>,
    preboot_cache: &mut Option<PathBuf>,
) {
    active_ui.take();
    preboot_web_ctx.take();
    if let Some(dir) = preboot_cache.take() {
        for _ in 0..4 {
            if std::fs::remove_dir_all(&dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
    }
}

fn build_main_runtime(
    el_target: &EventLoopWindowTarget<AppEvent>,
    win_icon: &WindowIcon,
    el_proxy: &EventLoopProxy<AppEvent>,
    paths: PortablePaths,
    cfg: LauncherConfig,
) -> Result<MainRuntime> {
    let webview2_cache = paths.cache_dir.join("webview2");
    std::fs::create_dir_all(&webview2_cache)?;

    let window = WindowBuilder::new()
        .with_title(PANEL_TITLE)
        .with_inner_size(LogicalSize::new(860.0, 560.0))
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_undecorated_shadow(false)
        .with_window_icon(Some(win_icon.clone()))
        .with_taskbar_icon(Some(win_icon.clone()))
        .build(el_target)
        .context("创建 WebView 宿主窗口失败")?;

    let html_content = render_panel_html();
    let mut web_context = WebContext::new(Some(webview2_cache));
    let ipc_proxy = el_proxy.clone();

    let webview = WebViewBuilder::new_with_web_context(&mut web_context)
        .with_transparent(true)
        .with_devtools(cfg!(debug_assertions))
        .with_html(html_content)
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_ipc_handler(move |req| {
            let body = req.into_body();
            let _ = ipc_proxy.send_event(AppEvent::Ipc(body));
        })
        .build(&window)
        .context("初始化 WebView2 界面引擎失败（请确认系统已安装 Edge WebView2 Runtime）")?;

    // 构建系统托盘与右键快捷菜单
    let tray_menu = Menu::new();
    let item_show = MenuItem::new("显示控制台面板", true, None);
    let item_launch = MenuItem::new("启动主环境 Antigravity IDE", true, None);
    let item_multi = MenuItem::new("多开新项目窗口...", true, None);
    let item_shell = MenuItem::new("打开便携 PowerShell", true, None);
    let item_check = MenuItem::new("Win 检测与升级...", true, None);
    let item_quit = MenuItem::new("退出 AideRun", true, None);

    let mut menu_map: HashMap<String, &'static str> = HashMap::new();
    menu_map.insert(item_show.id().0.clone(), "show");
    menu_map.insert(item_launch.id().0.clone(), "launch_main");
    menu_map.insert(item_multi.id().0.clone(), "multi_open");
    menu_map.insert(item_shell.id().0.clone(), "open_shell");
    menu_map.insert(item_check.id().0.clone(), "open_upgrade");
    menu_map.insert(item_quit.id().0.clone(), "quit");

    let _ = tray_menu.append(&item_show);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&item_launch);
    let _ = tray_menu.append(&item_multi);
    let _ = tray_menu.append(&item_shell);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&item_check);
    let _ = tray_menu.append(&PredefinedMenuItem::separator());
    let _ = tray_menu.append(&item_quit);

    let tray_img = TrayIconImg::from_rgba(TRAY_ICON_RGBA.to_vec(), 32, 32)
        .context("加载 32x32 托盘图标失败")?;
    let tray_icon = TrayIconBuilder::new()
        .with_tooltip(PANEL_TITLE)
        .with_icon(tray_img)
        .with_menu(Box::new(tray_menu))
        .build()
        .context("创建系统托盘图标失败")?;

    let tray_proxy = el_proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |ev| match ev {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            let _ = tray_proxy.send_event(AppEvent::TrayClick);
        }
        _ => {}
    }));

    let menu_proxy = el_proxy.clone();
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        if let Some(&act) = menu_map.get(&ev.id.0) {
            let _ = menu_proxy.send_event(AppEvent::MenuAction(act.to_string()));
        }
    }));

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shared = SharedApp {
        paths,
        cfg: Arc::new(Mutex::new(cfg)),
        proxy: el_proxy.clone(),
        update_cache: Arc::new(Mutex::new(None)),
        self_update_cache: Arc::new(Mutex::new(None)),
        remote_tool_vers: Arc::new(Mutex::new(HashMap::new())),
        busy_clean: Arc::new(AtomicBool::new(false)),
        busy_upgrade: Arc::new(AtomicBool::new(false)),
        busy_tools: Arc::new(AtomicBool::new(false)),
        busy_update: Arc::new(AtomicBool::new(false)),
        busy_self_update: Arc::new(AtomicBool::new(false)),
        smart_boot_checked: Arc::new(AtomicBool::new(false)),
        self_update_checked: Arc::new(AtomicBool::new(false)),
    };

    // 启动后台轻量守护线程：监听 IDE 进程退出后执行「程序智能清理」
    start_smart_clean_watcher(shared.clone(), shutdown_flag.clone());

    Ok(MainRuntime {
        webview,
        window,
        _web_context: web_context,
        tray_holder: Some(tray_icon),
        shutdown_flag,
        shared,
    })
}

fn prepare_ready_env_and_proxy(
    paths: &PortablePaths,
    cfg: &mut LauncherConfig,
    detected_proxy: Option<&ProxyEndpoint>,
    save_manual: bool,
) -> Result<()> {
    // 先将已知可用代理写入当前进程环境变量，确保原地解包下载组件可直接走代理
    apply_proxy_and_tools_to_self(paths, cfg, detected_proxy);
    ensure_portable_ready(paths, cfg, false)?;
    apply_confirmed_proxy_to_cfg(paths, cfg, detected_proxy, save_manual);
    Ok(())
}

/// 启动 860x560 水晶磨砂启动面板，并编排启动前置代理检测与便携环境 deploy 子窗口
pub fn run_panel() -> Result<()> {
    if try_focus_existing_panel() {
        return Ok(());
    }

    let paths = PortablePaths::discover()?;
    let mut cfg = LauncherConfig::load_or_default(&paths.launcher_config);

    // 启动前置静默检测（零目录写入）：优先扫常见 Win 翻墙端口与直连校验 Google，同时识别便携环境状态
    let probe_report = auto_detect_google_proxy(&paths, &cfg);
    let env_status = inspect_portable_env_status(&paths);

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let el_proxy = event_loop.create_proxy();
    let win_icon = WindowIcon::from_rgba(WIN_ICON_RGBA.to_vec(), 64, 64)
        .context("加载 64x64 窗口/任务栏图标失败")?;

    let mut preboot_cache: Option<PathBuf> = None;
    let mut preboot_web_ctx: Option<WebContext> = None;
    let mut pending_save_manual = false;
    let mut last_working_proxy: Option<ProxyEndpoint> = probe_report.endpoint.clone();

    let mut active_ui: Option<ActiveUi> =
        if probe_report.reachable && env_status != PortableEnvStatus::MissingIdeEnvironment {
            prepare_ready_env_and_proxy(&paths, &mut cfg, last_working_proxy.as_ref(), false)?;
            Some(ActiveUi::Main(build_main_runtime(
                &event_loop,
                &win_icon,
                &el_proxy,
                paths.clone(),
                cfg.clone(),
            )?))
        } else {
            // 将临时 WebView2 上下文严格隔离在系统 %TEMP%，用户确认部署前绝不污染当前目录
            let temp_dir =
                std::env::temp_dir().join(format!("AideRun_preboot_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&temp_dir);
            std::fs::create_dir_all(&temp_dir)?;
            let mut ctx = WebContext::new(Some(temp_dir.clone()));

            let session = if !probe_report.reachable {
                PrebootSession::open_proxy(
                    &event_loop,
                    &mut ctx,
                    &win_icon,
                    &el_proxy,
                    paths.clone(),
                    cfg.clone(),
                    &probe_report,
                )?
            } else {
                apply_proxy_and_tools_to_self(&paths, &cfg, last_working_proxy.as_ref());
                PrebootSession::open_deploy(
                    &event_loop,
                    &mut ctx,
                    &win_icon,
                    &el_proxy,
                    paths.clone(),
                    cfg.clone(),
                    false,
                )?
            };
            preboot_cache = Some(temp_dir);
            preboot_web_ctx = Some(ctx);
            Some(ActiveUi::Preboot(session))
        };

    event_loop.run(move |event, el_target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => match &mut active_ui {
                Some(ActiveUi::Preboot(pb)) if pb.window.id() == window_id => {
                    if !pb.is_deploying() {
                        cleanup_preboot_resources(
                            &mut active_ui,
                            &mut preboot_web_ctx,
                            &mut preboot_cache,
                        );
                        *control_flow = ControlFlow::Exit;
                    }
                }
                Some(ActiveUi::Main(main_rt)) if main_rt.window.id() == window_id => {
                    main_rt.shutdown_flag.store(true, AtomicOrdering::SeqCst);
                    main_rt.tray_holder.take();
                    active_ui.take();
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            },
            Event::UserEvent(app_ev) => match app_ev {
                AppEvent::EvalJs(js) => match &active_ui {
                    Some(ActiveUi::Preboot(pb)) => pb.eval_js(&js),
                    Some(ActiveUi::Main(main_rt)) => {
                        let _ = main_rt.webview.evaluate_script(&js);
                    }
                    None => {}
                },
                AppEvent::HideToTray => {
                    if let Some(ActiveUi::Main(main_rt)) = &active_ui {
                        main_rt.window.set_visible(false);
                    }
                }
                AppEvent::TrayClick => {
                    if let Some(ActiveUi::Main(main_rt)) = &active_ui {
                        restore_panel_window(&main_rt.window);
                        dispatch_worker(main_rt.shared.clone(), "init_state", Value::Null);
                    }
                }
                AppEvent::MenuAction(cmd) => {
                    if let Some(ActiveUi::Main(main_rt)) = &mut active_ui {
                        match cmd.as_str() {
                            "show" => {
                                restore_panel_window(&main_rt.window);
                                dispatch_worker(main_rt.shared.clone(), "init_state", Value::Null);
                            }
                            "launch_main" => {
                                dispatch_worker(main_rt.shared.clone(), "launch_main", Value::Null);
                            }
                            "multi_open" => {
                                dispatch_worker(
                                    main_rt.shared.clone(),
                                    "pick_and_launch_project",
                                    Value::Null,
                                );
                            }
                            "open_shell" => {
                                dispatch_worker(
                                    main_rt.shared.clone(),
                                    "open_shell",
                                    json!({ "powershell": true }),
                                );
                            }
                            "open_upgrade" => {
                                restore_panel_window(&main_rt.window);
                                emit_event_json(
                                    &main_rt.shared.proxy,
                                    json!({ "kind": "switch_tab", "tab": "upgrade" }),
                                );
                            }
                            "quit" => {
                                main_rt.shutdown_flag.store(true, AtomicOrdering::SeqCst);
                                main_rt.tray_holder.take();
                                active_ui.take();
                                *control_flow = ControlFlow::Exit;
                            }
                            _ => {}
                        }
                    }
                }
                AppEvent::Ipc(raw) => {
                    let Ok(msg) = serde_json::from_str::<IpcMessage>(&raw) else {
                        return;
                    };
                    match &mut active_ui {
                        Some(ActiveUi::Preboot(pb)) => {
                            if pb.handle_ipc(&msg.action, msg.payload) == PrebootIpcAction::QuitApp {
                                cleanup_preboot_resources(
                                    &mut active_ui,
                                    &mut preboot_web_ctx,
                                    &mut preboot_cache,
                                );
                                *control_flow = ControlFlow::Exit;
                            }
                        }
                        Some(ActiveUi::Main(main_rt)) => match msg.action.as_str() {
                            "drag_window" => {
                                let _ = main_rt.window.drag_window();
                            }
                            "minimize_to_tray" => {
                                main_rt.window.set_visible(false);
                            }
                            "quit_app" => {
                                main_rt.shutdown_flag.store(true, AtomicOrdering::SeqCst);
                                main_rt.tray_holder.take();
                                active_ui.take();
                                *control_flow = ControlFlow::Exit;
                            }
                            _ => dispatch_worker(main_rt.shared.clone(), &msg.action, msg.payload),
                        },
                        None => {}
                    }
                }
                AppEvent::PrebootProxyPassed {
                    endpoint,
                    save_manual,
                } => {
                    active_ui.take();
                    if endpoint.is_some() {
                        last_working_proxy = endpoint.clone();
                    }
                    if save_manual {
                        pending_save_manual = true;
                    }
                    apply_confirmed_proxy_to_cfg(
                        &paths,
                        &mut cfg,
                        last_working_proxy.as_ref(),
                        save_manual,
                    );

                    let next_env = inspect_portable_env_status(&paths);
                    if next_env == PortableEnvStatus::MissingIdeEnvironment {
                        if let Some(ctx) = preboot_web_ctx.as_mut() {
                            match PrebootSession::open_deploy(
                                el_target,
                                ctx,
                                &win_icon,
                                &el_proxy,
                                paths.clone(),
                                cfg.clone(),
                                pending_save_manual,
                            ) {
                                Ok(deploy_pb) => active_ui = Some(ActiveUi::Preboot(deploy_pb)),
                                Err(err) => {
                                    cleanup_preboot_resources(
                                        &mut active_ui,
                                        &mut preboot_web_ctx,
                                        &mut preboot_cache,
                                    );
                                    show_error_dialog("AideRun 初始化失败", &format!("{err:#}"));
                                    *control_flow = ControlFlow::Exit;
                                }
                            }
                        }
                    } else {
                        cleanup_preboot_resources(
                            &mut active_ui,
                            &mut preboot_web_ctx,
                            &mut preboot_cache,
                        );
                        match prepare_ready_env_and_proxy(
                            &paths,
                            &mut cfg,
                            last_working_proxy.as_ref(),
                            pending_save_manual,
                        )
                        .and_then(|()| {
                            build_main_runtime(
                                el_target,
                                &win_icon,
                                &el_proxy,
                                paths.clone(),
                                cfg.clone(),
                            )
                        }) {
                            Ok(main_rt) => active_ui = Some(ActiveUi::Main(main_rt)),
                            Err(err) => {
                                show_error_dialog("AideRun 启动失败", &format!("{err:#}"));
                                *control_flow = ControlFlow::Exit;
                            }
                        }
                    }
                }
                AppEvent::PrebootDeployCompleted => {
                    cleanup_preboot_resources(
                        &mut active_ui,
                        &mut preboot_web_ctx,
                        &mut preboot_cache,
                    );
                    cfg = LauncherConfig::load_or_default(&paths.launcher_config);
                    apply_confirmed_proxy_to_cfg(
                        &paths,
                        &mut cfg,
                        last_working_proxy.as_ref(),
                        false,
                    );
                    match build_main_runtime(
                        el_target,
                        &win_icon,
                        &el_proxy,
                        paths.clone(),
                        cfg.clone(),
                    ) {
                        Ok(main_rt) => active_ui = Some(ActiveUi::Main(main_rt)),
                        Err(err) => {
                            show_error_dialog("AideRun 主控制台启动失败", &format!("{err:#}"));
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                }
                AppEvent::SelfUpdateRestart => {
                    if let Some(ActiveUi::Main(mut main_rt)) = active_ui.take() {
                        main_rt.shutdown_flag.store(true, AtomicOrdering::SeqCst);
                        main_rt.window.set_visible(false);
                        main_rt.tray_holder.take();
                        drop(main_rt);
                    }
                    if let Err(err) = spawn_new_self_after_update() {
                        show_error_dialog(
                            "AideRun 重启失败",
                            &format!("新版文件已替换成功，但自动重启失败：{err:#}"),
                        );
                    }
                    *control_flow = ControlFlow::Exit;
                }
            },
            _ => {}
        }
    });
}

fn start_smart_clean_watcher(app: SharedApp, shutdown: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut was_running = !find_blocking_ide_processes(&app.paths.root).is_empty();
        while !shutdown.load(AtomicOrdering::SeqCst) {
            for _ in 0..12 {
                if shutdown.load(AtomicOrdering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            let now_running = !find_blocking_ide_processes(&app.paths.root).is_empty();
            if was_running && !now_running {
                let cfg = read_cfg(&app);
                if cfg.launch.smart_clean {
                    if let Ok(Some(sum)) = run_smart_daily_clean_if_due(&app.paths, &cfg, true) {
                        if sum.bytes_freed > 0 {
                            emit_status(
                                &app.proxy,
                                "ok",
                                &format!(
                                    "程序智能清理：IDE 已退出，自动释放 {} 日常缓存",
                                    format_bytes(sum.bytes_freed)
                                ),
                                None,
                            );
                            push_state(&app);
                        }
                    }
                }
            }
            was_running = now_running;
        }
    });
}

fn restore_panel_window(window: &tao::window::Window) {
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
}

fn dispatch_worker(app: SharedApp, action: &str, payload: Value) {
    let act = action.to_string();
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_worker_action(&app, &act, payload);
        }));
        if res.is_err() {
            emit_status(
                &app.proxy,
                "err",
                &format!("后台操作 [{act}] 遇到意外异常，详情已记录至 data/cache/logs/panic.log"),
                None,
            );
        }
    });
}

fn handle_worker_action(app: &SharedApp, action: &str, payload: Value) {
    match action {
        "init_state" => {
            if !app.smart_boot_checked.swap(true, AtomicOrdering::SeqCst) {
                let cfg = read_cfg(app);
                if cfg.launch.smart_clean {
                    if let Ok(Some(sum)) = run_smart_daily_clean_if_due(&app.paths, &cfg, false) {
                        if sum.bytes_freed > 0 {
                            emit_status(
                                &app.proxy,
                                "ok",
                                &format!(
                                    "程序智能清理：启动检测已自动释放 {} 日常垃圾",
                                    format_bytes(sum.bytes_freed)
                                ),
                                None,
                            );
                        }
                    }
                }
            }
            push_state(app);

            // 启动主面板后在后台静默检查一次 AideRun 本体新版本（发现新版触发底栏呼吸提醒）
            if !app.self_update_checked.swap(true, AtomicOrdering::SeqCst) {
                let app_bg = app.clone();
                std::thread::spawn(move || {
                    run_self_update_check(&app_bg, false);
                });
            } else if let Some(cached) = app
                .self_update_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                emit_self_update_result(&app.proxy, &cached, true);
            }
        }
        "launch_main" => {
            emit_status(&app.proxy, "busy", "正在准备便携环境并启动 Antigravity IDE...", None);
            let cfg = read_cfg(app);
            match launch_ide_new_window(&app.paths, &cfg, None, false) {
                Ok(()) => {
                    emit_status(&app.proxy, "ok", "已启动主环境 Antigravity IDE（面板自动收起至托盘）", None);
                    let _ = app.proxy.send_event(AppEvent::HideToTray);
                }
                Err(e) => emit_status(&app.proxy, "err", &format!("启动失败: {e:#}"), None),
            }
        }
        "launch_project" => {
            let path_str = payload.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let new_win = payload.get("new_window").and_then(|v| v.as_bool()).unwrap_or(true);
            let target = PathBuf::from(path_str);
            emit_status(&app.proxy, "busy", &format!("正在多开项目窗口: {path_str}"), None);
            let cfg = read_cfg(app);
            match launch_ide_new_window(&app.paths, &cfg, Some(&target), new_win) {
                Ok(()) => {
                    emit_status(&app.proxy, "ok", "项目新窗口已启动（面板收起至托盘）", None);
                    let _ = app.proxy.send_event(AppEvent::HideToTray);
                }
                Err(e) => emit_status(&app.proxy, "err", &format!("项目多开失败: {e:#}"), None),
            }
        }
        "pick_and_launch_project" => {
            let init_dir = if app.paths.projects_dir.is_dir() {
                app.paths.projects_dir.clone()
            } else {
                app.paths.root.clone()
            };
            if let Some(dir) = pick_folder_dialog("选择要在独立窗口打开的项目目录", &init_dir) {
                let cfg = read_cfg(app);
                match launch_ide_new_window(&app.paths, &cfg, Some(&dir), true) {
                    Ok(()) => {
                        emit_status(
                            &app.proxy,
                            "ok",
                            &format!("已多开窗口: {}", dir.display()),
                            None,
                        );
                        let _ = app.proxy.send_event(AppEvent::HideToTray);
                    }
                    Err(e) => emit_status(&app.proxy, "err", &format!("多开失败: {e:#}"), None),
                }
            }
        }
        "launch_instance" => {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let share_ext = payload.get("share_ext").and_then(|v| v.as_bool()).unwrap_or(true);
            let cfg = read_cfg(app);
            match launch_ide_isolated_instance(&app.paths, &cfg, name, share_ext, None) {
                Ok(safe) => {
                    emit_status(&app.proxy, "ok", &format!("已启动独立隔离分身 [{safe}]"), None);
                    push_state(app);
                    let _ = app.proxy.send_event(AppEvent::HideToTray);
                }
                Err(e) => emit_status(&app.proxy, "err", &format!("分身启动失败: {e:#}"), None),
            }
        }
        "open_shell" => {
            let ps = payload.get("powershell").and_then(|v| v.as_bool()).unwrap_or(true);
            let cfg = read_cfg(app);
            match spawn_portable_shell_window(&app.paths, &cfg, ps) {
                Ok(()) => emit_status(
                    &app.proxy,
                    "ok",
                    if ps { "已打开便携 PowerShell" } else { "已打开便携 CMD" },
                    None,
                ),
                Err(e) => emit_status(&app.proxy, "err", &format!("打开终端失败: {e:#}"), None),
            }
        }
        "create_shortcut" => {
            match create_desktop_shortcut(&app.paths) {
                Ok(()) => emit_status(&app.proxy, "ok", "已在桌面创建 Antigravity便携版.lnk", None),
                Err(e) => emit_status(&app.proxy, "err", &format!("快捷方式创建失败: {e:#}"), None),
            }
        }
        "toggle_setting" => {
            let auto_clean_req = payload.get("auto_clean_on_exit").and_then(|x| x.as_bool());
            let smart_clean_req = payload.get("smart_clean").and_then(|x| x.as_bool());
            let cfg = update_cfg(app, |c| {
                if let Some(v) = auto_clean_req {
                    c.launch.auto_clean_on_exit = v;
                }
                if let Some(v) = smart_clean_req {
                    c.launch.smart_clean = v;
                }
            });
            if let Err(e) = cfg.save(&app.paths.launcher_config) {
                emit_status(&app.proxy, "err", &format!("保存配置失败: {e:#}"), None);
                return;
            }
            if smart_clean_req == Some(true) {
                emit_status(&app.proxy, "busy", "已开启程序智能清理，正在巡检日常垃圾...", None);
                match run_smart_daily_clean_if_due(&app.paths, &cfg, true) {
                    Ok(Some(sum)) if sum.bytes_freed > 0 => {
                        emit_status(
                            &app.proxy,
                            "ok",
                            &format!(
                                "已开启程序智能清理，并即时释放 {} 日常垃圾",
                                format_bytes(sum.bytes_freed)
                            ),
                            None,
                        );
                    }
                    Ok(_) => {
                        emit_status(
                            &app.proxy,
                            "ok",
                            "已开启程序智能清理（将在 IDE 退出或超 200MB 时自动清理日常垃圾）",
                            None,
                        );
                    }
                    Err(e) => emit_status(&app.proxy, "warn", &format!("已保存设置，即时清理提示: {e:#}"), None),
                }
                push_state(app);
            } else {
                emit_status(&app.proxy, "ok", "已保存设置至 data/launcher.toml", None);
            }
        }
        "run_clean_mode" => {
            let Some(_guard) = BusyGuard::try_acquire(&app.busy_clean) else {
                emit_status(&app.proxy, "warn", "清理任务正在执行中，请稍候...", None);
                return;
            };
            let mode = payload.get("mode").and_then(|v| v.as_str()).unwrap_or("daily");
            let include_sanitize = payload
                .get("include_sanitize")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let cfg = read_cfg(app);

            if mode == "pack" {
                emit_status(&app.proxy, "busy", "正在执行打包前彻底清理...", None);
                match run_clean_with_opts(&app.paths, &cfg.cleanup, CleanLevel::All, false, true, false, false) {
                    Ok(sum) => {
                        let mut final_msg = format!(
                            "彻底清理完成：释放 {}（清理 {} 项）",
                            format_bytes(sum.bytes_freed),
                            sum.items_cleaned
                        );
                        if include_sanitize {
                            emit_status(&app.proxy, "busy", "正在执行账号登录态与对话历史隐私脱敏...", None);
                            match run_sanitize(&app.paths, &cfg.cleanup, false, true) {
                                Ok(rep) => {
                                    final_msg = format!(
                                        "打包清理就绪：释放 {}，清理数据库 {} 条敏感记录与 {} 个历史目录（保留中文与免权限设置）",
                                        format_bytes(sum.bytes_freed),
                                        rep.db_rows_deleted,
                                        rep.dirs_cleared
                                    );
                                }
                                Err(e) => {
                                    emit_status(&app.proxy, "err", &format!("隐私脱敏失败: {e:#}"), None);
                                    push_state(app);
                                    return;
                                }
                            }
                        }
                        emit_status(&app.proxy, "ok", &final_msg, None);
                        push_state(app);
                    }
                    Err(e) => emit_status(&app.proxy, "err", &format!("{e:#}"), None),
                }
            } else {
                emit_status(&app.proxy, "busy", "正在执行一键日常垃圾清理（推荐）...", None);
                match run_clean_with_opts(&app.paths, &cfg.cleanup, CleanLevel::Deep, false, true, false, false) {
                    Ok(sum) => {
                        let msg = format!(
                            "日常垃圾清理完成：释放 {}（清理 {} 项，跳过占用 {} 项）",
                            format_bytes(sum.bytes_freed),
                            sum.items_cleaned,
                            sum.skipped_locked
                        );
                        emit_status(&app.proxy, "ok", &msg, None);
                        push_state(app);
                    }
                    Err(e) => emit_status(&app.proxy, "err", &format!("{e:#}"), None),
                }
            }
        }
        "check_update" => {
            // 同一次 AideRun.exe 运行期间仅检测一次并缓存结果
            if let Some(cached) = app
                .update_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                emit_cached_update(&app.proxy, &cached);
                return;
            }
            let Some(_guard) = BusyGuard::try_acquire(&app.busy_update) else {
                return;
            };

            emit_status(
                &app.proxy,
                "busy",
                "正在自动检测 Windows x64 官方发布版 (antigravity.google)...",
                Some("upg-log-box"),
            );
            let cfg = read_cfg(app);
            let cached = match check_latest_release(&app.paths, &cfg.upgrade) {
                Ok(r) => CachedUpdateCheck {
                    ok: true,
                    local_ver: r.local_ide_version,
                    remote_ver: Some(r.remote_version),
                    download_url: Some(r.download_url),
                    source_label: Some(r.source_label),
                    has_update: r.has_update,
                    error: None,
                },
                Err(e) => CachedUpdateCheck {
                    ok: false,
                    local_ver: read_local_versions(&app.paths).0,
                    remote_ver: None,
                    download_url: None,
                    source_label: None,
                    has_update: false,
                    error: Some(format!("{e:#}")),
                },
            };

            *app.update_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(cached.clone());
            emit_cached_update(&app.proxy, &cached);
        }
        "pick_local_installer" => {
            if let Some(p) = pick_installer_file_dialog(&app.paths.root) {
                let path_str = p.display().to_string();
                emit_event_json(
                    &app.proxy,
                    json!({ "kind": "local_installer_picked", "path": path_str }),
                );
                emit_status(
                    &app.proxy,
                    "ok",
                    "已选择本地安装包，请点击「开始本地安装包升级」执行无痕升级",
                    Some("upg-log-box"),
                );
            } else {
                emit_status(&app.proxy, "warn", "已取消选择本地安装包", None);
            }
        }
        "run_upgrade" => {
            let Some(_guard) = BusyGuard::try_acquire(&app.busy_upgrade) else {
                emit_status(&app.proxy, "warn", "升级任务正在执行中，请稍候...", Some("upg-log-box"));
                return;
            };
            let req_src = payload
                .get("source")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let picked = match req_src.as_deref() {
                Some("__pick__") => match pick_installer_file_dialog(&app.paths.root) {
                    Some(p) => Some(p.display().to_string()),
                    None => {
                        emit_status(&app.proxy, "warn", "已取消选择本地安装包", Some("upg-log-box"));
                        emit_event_json(&app.proxy, json!({ "kind": "upgrade_done", "ok": true, "cancelled": true }));
                        return;
                    }
                },
                other => other.map(str::to_string),
            };
            // 纵深防御：若为在线升级模式且已知线上版本号未大于本地版，拒绝执行
            if picked.is_none() {
                let cached_has_update = app
                    .update_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|c| c.has_update);
                if cached_has_update == Some(false) {
                    emit_status(&app.proxy, "warn", "当前线上版本未高于本地版本，请先选择本地安装包", Some("upg-log-box"));
                    emit_event_json(&app.proxy, json!({ "kind": "upgrade_done", "ok": false, "up_to_date": true }));
                    return;
                }
            }
            emit_status(
                &app.proxy,
                "busy",
                "正在使用 innounp-2 零注册表解包并执行白名单原子替换...",
                Some("upg-log-box"),
            );
            let cfg = read_cfg(app);
            match execute_upgrade(picked.as_deref(), true, &app.paths, &cfg) {
                Ok(out) => {
                    let msg = if out.cancelled {
                        "已取消本次升级".to_string()
                    } else if out.up_to_date {
                        "当前已是最新稳定版，无需升级".to_string()
                    } else {
                        sync_update_cache_after_local_change(app, out.new_version.clone());
                        format!(
                            "升级成功：{} -> {}（data/projects/tools/proxy 全部无损保留）",
                            out.old_version.as_deref().unwrap_or("旧版"),
                            out.new_version.as_deref().unwrap_or("新版")
                        )
                    };
                    emit_status(&app.proxy, "ok", &msg, Some("upg-log-box"));
                    emit_event_json(
                        &app.proxy,
                        json!({
                            "kind": "upgrade_done",
                            "ok": true,
                            "cancelled": out.cancelled,
                            "up_to_date": out.up_to_date
                        }),
                    );
                    push_state(app);
                }
                Err(e) => {
                    emit_status(&app.proxy, "err", &format!("升级失败: {e:#}"), Some("upg-log-box"));
                    emit_event_json(&app.proxy, json!({ "kind": "upgrade_done", "ok": false }));
                }
            }
        }
        "rollback_upgrade" => {
            let Some(_guard) = BusyGuard::try_acquire(&app.busy_upgrade) else {
                emit_status(&app.proxy, "warn", "升级任务正在执行中，请稍候...", Some("upg-log-box"));
                return;
            };
            emit_status(&app.proxy, "busy", "正在回滚至升级前内核快照...", Some("upg-log-box"));
            match rollback_upgrade(&app.paths) {
                Ok(()) => {
                    sync_update_cache_after_local_change(app, None);
                    emit_status(&app.proxy, "ok", "已成功回滚至上一版本内核", Some("upg-log-box"));
                    push_state(app);
                }
                Err(e) => emit_status(&app.proxy, "err", &format!("回滚失败: {e:#}"), Some("upg-log-box")),
            }
        }
        "refresh_components" => {
            let fetch_remote = payload
                .get("fetch_remote")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let cfg = read_cfg(app);
            if fetch_remote {
                emit_status(&app.proxy, "busy", "正在并发检测各组件官方最新版本...", None);
            }
            let cached_map = app
                .remote_tool_vers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let list = collect_component_statuses(
                &app.paths,
                &cfg,
                fetch_remote,
                Some(&cached_map),
            );
            if fetch_remote {
                let mut map_guard = app
                    .remote_tool_vers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                for item in &list {
                    if let Some(ref rv) = item.remote_version {
                        if item.builtin {
                            map_guard.insert(item.id.clone(), rv.clone());
                        }
                    }
                }
                emit_status(&app.proxy, "ok", "组件状态与版本对比已刷新", None);
            }
            emit_event_json(&app.proxy, json!({ "kind": "components_data", "components": list }));
        }
        "component_action" => {
            let Some(_guard) = BusyGuard::try_acquire(&app.busy_tools) else {
                emit_status(&app.proxy, "warn", "另一个组件操作正在进行中，请稍候...", None);
                return;
            };
            let op = payload.get("op").and_then(|v| v.as_str()).unwrap_or("install");
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.trim().is_empty() {
                return;
            }
            match op {
                "uninstall" => {
                    emit_status(&app.proxy, "busy", &format!("正在卸载组件 {name}..."), None);
                    let mut cfg_snapshot = read_cfg(app);
                    match remove_portable_tool(&app.paths, &mut cfg_snapshot, name) {
                        Ok(()) => {
                            let saved_cfg = cfg_snapshot.clone();
                            *app.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg_snapshot;
                            emit_components_snapshot(app, &saved_cfg);
                            emit_status(&app.proxy, "ok", &format!("已卸载组件 {name}"), None);
                            push_state(app);
                        }
                        Err(e) => emit_status(&app.proxy, "err", &format!("卸载 {name} 失败: {e:#}"), None),
                    }
                }
                _ => {
                    let action_cn = if op == "upgrade" { "升级" } else { "安装" };
                    emit_status(
                        &app.proxy,
                        "busy",
                        &format!("正在{action_cn}原生零软链接组件: {name} ..."),
                        None,
                    );
                    let cfg = read_cfg(app);
                    match install_portable_tool(&app.paths, &cfg, name, None) {
                        Ok(()) => {
                            emit_components_snapshot(app, &cfg);
                            emit_status(&app.proxy, "ok", &format!("组件 {name} {action_cn}就绪"), None);
                            push_state(app);
                        }
                        Err(e) => emit_status(&app.proxy, "err", &format!("{action_cn} {name} 失败: {e:#}"), None),
                    }
                }
            }
        }
        "add_custom_component" => {
            let Some(_guard) = BusyGuard::try_acquire(&app.busy_tools) else {
                emit_status(&app.proxy, "warn", "另一个组件操作正在进行中，请稍候...", None);
                return;
            };
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let desc = payload.get("desc").and_then(|v| v.as_str()).map(str::to_string);
            let mut cfg_snapshot = read_cfg(app);
            if let Err(e) = add_custom_component(&app.paths, &mut cfg_snapshot, name, url, None, desc) {
                emit_status(&app.proxy, "err", &format!("添加失败: {e:#}"), None);
                return;
            }
            let saved_cfg = cfg_snapshot.clone();
            *app.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg_snapshot;

            emit_status(&app.proxy, "busy", &format!("正在下载部署自定义组件 {name}..."), None);
            if let Err(e) = install_portable_tool(&app.paths, &saved_cfg, name, Some(url.trim())) {
                emit_status(&app.proxy, "err", &format!("部署 {name} 失败: {e:#}"), None);
                return;
            }
            emit_components_snapshot(app, &saved_cfg);
            emit_status(&app.proxy, "ok", &format!("自定义组件 {name} 添加并部署完成"), None);
            push_state(app);
        }
        "install_preset" => {
            let preset = payload.get("preset").and_then(|v| v.as_str()).unwrap_or("");
            install_tool_preset(app, preset);
        }
        "refresh_doctor" => {
            let cfg = read_cfg(app);
            let report = collect_doctor_report(&app.paths, &cfg);
            emit_event_json(&app.proxy, json!({ "kind": "doctor_report", "report": report }));
        }
        "open_github_repo" => {
            if let Err(err) = open_github_repository() {
                emit_status(&app.proxy, "err", &format!("打开 GitHub 页面失败: {err:#}"), None);
            }
        }
        "check_self_update" => {
            let force = payload.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            if !force {
                if let Some(cached) = app
                    .self_update_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                {
                    emit_self_update_result(&app.proxy, &cached, false);
                    return;
                }
            }
            run_self_update_check(app, true);
        }
        "run_self_update" => {
            let Some(_guard) = BusyGuard::try_acquire(&app.busy_self_update) else {
                emit_status(&app.proxy, "warn", "AideRun 自身升级正在执行中...", None);
                return;
            };
            let explicit_url = payload
                .get("url")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let cached_url = app
                .self_update_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|r| r.download_url.clone());

            let target_url = match explicit_url.or(cached_url) {
                Some(u) => u,
                None => match check_self_update() {
                    Ok(res) if res.has_update => res.download_url,
                    Ok(res) => {
                        emit_event_json(
                            &app.proxy,
                            json!({
                                "kind": "self_update_failed",
                                "error": format!("当前已是最新版本 v{}", res.local_version)
                            }),
                        );
                        return;
                    }
                    Err(err) => {
                        emit_event_json(
                            &app.proxy,
                            json!({
                                "kind": "self_update_failed",
                                "error": format!("{err:#}")
                            }),
                        );
                        return;
                    }
                },
            };

            let proxy_ref = app.proxy.clone();
            match download_and_swap_self(&target_url, |pct, msg| {
                emit_event_json(
                    &proxy_ref,
                    json!({
                        "kind": "self_update_progress",
                        "percent": pct,
                        "text": msg
                    }),
                );
            }) {
                Ok(()) => {
                    std::thread::sleep(Duration::from_millis(320));
                    let _ = app.proxy.send_event(AppEvent::SelfUpdateRestart);
                }
                Err(err) => {
                    let msg = format!("{err:#}");
                    emit_status(&app.proxy, "err", &format!("AideRun 自升级失败: {msg}"), None);
                    emit_event_json(
                        &app.proxy,
                        json!({
                            "kind": "self_update_failed",
                            "error": msg
                        }),
                    );
                }
            }
        }
        _ => {}
    }
}

fn emit_self_update_result(
    proxy: &EventLoopProxy<AppEvent>,
    res: &SelfUpdateCheckResult,
    silent: bool,
) {
    emit_event_json(
        proxy,
        json!({
            "kind": "self_update_result",
            "ok": true,
            "silent": silent,
            "data": res
        }),
    );
}

fn run_self_update_check(app: &SharedApp, manual: bool) {
    if manual {
        emit_status(&app.proxy, "busy", "正在检测 AideRun 启动器最新版本...", None);
    }
    match check_self_update() {
        Ok(res) => {
            *app.self_update_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(res.clone());
            emit_self_update_result(&app.proxy, &res, !manual);
            if manual {
                if res.has_update {
                    emit_status(
                        &app.proxy,
                        "warn",
                        &format!(
                            "发现 AideRun 新版本 v{}（当前 v{}），可点击无感升级",
                            res.remote_version, res.local_version
                        ),
                        None,
                    );
                } else {
                    emit_status(
                        &app.proxy,
                        "ok",
                        &format!("AideRun 启动器已是最新版 v{}", res.local_version),
                        None,
                    );
                }
            }
        }
        Err(err) => {
            let err_msg = format!("{err:#}");
            emit_event_json(
                &app.proxy,
                json!({
                    "kind": "self_update_result",
                    "ok": false,
                    "silent": !manual,
                    "error": err_msg
                }),
            );
            if manual {
                emit_status(&app.proxy, "err", &format!("检测 AideRun 自更新失败: {err_msg}"), None);
            }
        }
    }
}

fn emit_cached_update(proxy: &EventLoopProxy<AppEvent>, c: &CachedUpdateCheck) {
    if c.ok {
        emit_event_json(
            proxy,
            json!({
                "kind": "update_result",
                "ok": true,
                "data": {
                    "local_ver": c.local_ver,
                    "remote_ver": c.remote_ver,
                    "download_url": c.download_url,
                    "source_label": c.source_label,
                    "has_update": c.has_update
                }
            }),
        );
    } else {
        emit_event_json(
            proxy,
            json!({
                "kind": "update_result",
                "ok": false,
                "error": c.error.as_deref().unwrap_or("检测失败")
            }),
        );
    }
}

fn push_state(app: &SharedApp) {
    let cfg = read_cfg(app);
    let cached_remote = app
        .remote_tool_vers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let state = build_dashboard_state(&app.paths, &cfg, &cached_remote);
    emit_event_json(&app.proxy, json!({ "kind": "state", "state": state }));
}

fn build_dashboard_state(
    paths: &PortablePaths,
    cfg: &LauncherConfig,
    cached_remote: &HashMap<String, String>,
) -> DashboardState {
    let ide_installed = paths.ide_exe.is_file();
    let ide_needs_unpack =
        ide_installed && !paths.resources_dir.is_dir() && is_inno_setup_installer(&paths.ide_exe);
    let (ide_version, _) = read_local_versions(paths);

    let proxy_text = if let Some(ep) = read_config_json_proxy(&paths.proxy_config) {
        let ok = check_proxy_reachable(&ep);
        format!("{} ({})", ep.all_proxy_url(), if ok { "通畅" } else { "未通" })
    } else if let Some(sys) = read_windows_system_proxy() {
        let ok = check_proxy_reachable(&sys);
        format!("系统 {} ({})", sys.all_proxy_url(), if ok { "通畅" } else { "未通" })
    } else {
        "直连模式（未发现活跃代理）".to_string()
    };

    let active_procs = find_active_processes_in_root(&paths.root)
        .into_iter()
        .map(|p| ActiveProcItem { pid: p.pid, name: p.name })
        .collect();

    let projects = list_project_folders(paths)
        .into_iter()
        .map(|(name, path)| ProjectItem { name, path })
        .collect();

    let instances = list_isolated_instances(paths);
    let (data_bytes, _) = measure_path_size(&paths.data_dir);
    let (daily_clean_fmt, pack_clean_fmt, junk_breakdown) =
        scan_clean_breakdown(paths, &cfg.cleanup);

    let innounp_ready = paths.portable_tools_dir.join("innounp").join("innounp.exe").is_file()
        || paths.tools_dir.join("innounp.exe").is_file();

    let components = collect_component_statuses(paths, cfg, false, Some(cached_remote));

    DashboardState {
        aiderun_version: env!("CARGO_PKG_VERSION"),
        root_dir: paths.root.display().to_string(),
        ide_installed,
        ide_needs_unpack,
        ide_version,
        needs_bootstrap: paths.needs_portable_bootstrap(),
        proxy_text,
        ntfs: is_ntfs_volume(&paths.root),
        active_procs,
        auto_clean_on_exit: cfg.launch.auto_clean_on_exit,
        smart_clean: cfg.launch.smart_clean,
        projects,
        instances,
        data_total_fmt: format_bytes(data_bytes),
        daily_clean_fmt,
        pack_clean_fmt,
        junk_breakdown,
        innounp_ready,
        components,
    }
}

fn emit_status(proxy: &EventLoopProxy<AppEvent>, level: &str, text: &str, log_box: Option<&str>) {
    emit_event_json(
        proxy,
        json!({
            "kind": "status",
            "level": level,
            "text": text,
            "log_box": log_box
        }),
    );
}

pub(crate) fn emit_event_json(proxy: &EventLoopProxy<AppEvent>, val: Value) {
    let serialized = val
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let js = format!("window.__onAideEvent && window.__onAideEvent({serialized});");
    let _ = proxy.send_event(AppEvent::EvalJs(js));
}

pub(crate) fn make_brand_logo_data_uri() -> String {
    let encoded_svg = percent_encode_svg(BRAND_LOGO_SVG);
    format!("data:image/svg+xml;charset=utf-8,{encoded_svg}")
}

fn render_panel_html() -> String {
    PANEL_HTML_TEMPLATE.replace("{{BRAND_LOGO_DATA_URI}}", &make_brand_logo_data_uri())
}

pub(crate) fn percent_encode_svg(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len() + 256);
    for ch in svg.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '"' => out.push_str("%22"),
            '\r' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// 解析新手场景预设 ID 对应的中文名称及组件安装序列
fn resolve_tool_preset(preset: &str) -> Option<(&'static str, &'static [&'static str])> {
    match preset {
        "all" => Some(("全能开发推荐", &["innounp", "uv", "7zip", "mingit", "nodejs"])),
        "python" => Some(("Python / AI 开发", &["uv", "mingit", "7zip"])),
        "web" => Some(("前端全栈开发", &["nodejs", "mingit", "7zip"])),
        "basic" => Some(("基础便携装机", &["innounp", "mingit", "7zip"])),
        _ => None,
    }
}

/// 刷新并向前端推送最新组件状态表格快照
fn emit_components_snapshot(app: &SharedApp, cfg: &LauncherConfig) -> Vec<ComponentStatusItem> {
    let cached_map = app
        .remote_tool_vers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let list = collect_component_statuses(&app.paths, cfg, false, Some(&cached_map));
    emit_event_json(&app.proxy, json!({ "kind": "components_data", "components": list }));
    list
}

/// 在升级或回滚改变本地 IDE 版本后重算在线更新缓存并推送前端
fn sync_update_cache_after_local_change(app: &SharedApp, new_local: Option<String>) {
    let resolved_local = new_local.or_else(|| read_local_versions(&app.paths).0);
    let updated = {
        let Ok(mut guard) = app.update_cache.lock() else {
            return;
        };
        if let Some(ref mut c) = *guard {
            c.local_ver = resolved_local.clone();
            c.has_update = match (c.remote_ver.as_deref(), resolved_local.as_deref()) {
                (Some(r), Some(l)) => compare_semver(r, l) == std::cmp::Ordering::Greater,
                _ => false,
            };
            Some(c.clone())
        } else {
            None
        }
    };
    if let Some(c) = updated {
        emit_cached_update(&app.proxy, &c);
    }
}

/// 执行新手开发场景预设：跳过已安装最新组件，按序部署缺失或可升级组件
fn install_tool_preset(app: &SharedApp, preset: &str) {
    let Some(_guard) = BusyGuard::try_acquire(&app.busy_tools) else {
        emit_status(&app.proxy, "warn", "另一个组件操作正在进行中，请稍候...", None);
        return;
    };
    let Some((label, ids)) = resolve_tool_preset(preset) else {
        emit_status(&app.proxy, "err", &format!("未知的预设方案: {preset}"), None);
        return;
    };

    let cfg = read_cfg(app);
    let current = emit_components_snapshot(app, &cfg);
    let pending: Vec<(&str, &str)> = ids
        .iter()
        .filter_map(|&id| {
            let item = current.iter().find(|c| c.id == id);
            match item {
                Some(it) if it.installed && it.update_state != "updatable" => None,
                Some(it) if it.installed => Some((id, "升级")),
                _ => Some((id, "安装")),
            }
        })
        .collect();

    if pending.is_empty() {
        emit_status(
            &app.proxy,
            "ok",
            &format!("预设「{label}」包含的 {} 项组件均已就绪，无需重复下载", ids.len()),
            None,
        );
        return;
    }

    let skipped = ids.len() - pending.len();
    let total = pending.len();
    let mut ok_count = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (idx, (id, op_cn)) in pending.into_iter().enumerate() {
        emit_status(
            &app.proxy,
            "busy",
            &format!("[{}/{}] 正在{op_cn}预设「{label}」组件: {id} ...", idx + 1, total),
            None,
        );
        match install_portable_tool(&app.paths, &cfg, id, None) {
            Ok(()) => {
                ok_count += 1;
                emit_components_snapshot(app, &cfg);
            }
            Err(e) => failures.push(format!("{id}: {e:#}")),
        }
    }

    push_state(app);
    if failures.is_empty() {
        emit_status(
            &app.proxy,
            "ok",
            &format!("预设「{label}」一键配齐完成：成功 {ok_count} 项（已跳过 {skipped} 项就绪组件）"),
            None,
        );
    } else {
        emit_status(
            &app.proxy,
            "err",
            &format!(
                "预设「{label}」部分完成：成功 {ok_count}/{total} 项，失败: {}",
                failures.join("; ")
            ),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_tool_preset;
    use crate::toolbox::BUILTIN_PORTABLE_TOOLS;

    #[test]
    fn preset_definitions_reference_valid_builtin_panel_tools() {
        for key in ["all", "python", "web", "basic"] {
            let (label, ids) = resolve_tool_preset(key).expect("预设应存在");
            assert!(!label.is_empty());
            assert!(!ids.is_empty());
            for id in ids {
                assert!(
                    BUILTIN_PORTABLE_TOOLS
                        .iter()
                        .any(|t| t.name == *id && t.show_in_panel),
                    "预设 {key} 引用了无效或未展示组件: {id}"
                );
            }
        }
        assert!(resolve_tool_preset("nonexistent").is_none());
    }
}
