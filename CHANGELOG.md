<!-- AideRun - 便携版一体化启动器更新日志（简体中文） -->
# 更新日志 (AideRun)

## [1.3.0] - 2026-11-12

### 新增与改进
- **启动前置双引导子窗口架构**：
  - **独立窗口一（网络代理自检与配置，`480x410`）**：GUI 启动前无副作用并发扫描常见 Windows 代理软件本地端口（Clash `7890`/`7897`、v2rayN `10809`/`10808`、Shadowsocks `1080`、NekoBox `2080`、Hiddify 等）与系统代理/TUN 直连状态并实测 Google 连通性；检测通过则静默进入下一环节，未通过则弹出水晶玻璃拟态子窗口支持客户端预设快速填入、粘贴完整代理 URL、手动填写端口与实时连通验证。
  - **独立窗口二（便携环境识别与官方一键部署，`500x360`）**：网络就绪后无侵入检测当前目录是否存在 Antigravity IDE 环境。若已有便携环境（或待解包官方单文件安装包）则直接进入主面板；若缺失 IDE（如仅存在单个 `AideRun.exe`）则弹出明确中文确认提示（“是，立即下载部署” / “否”）：选择“是”则在子窗口内实时显示下载进度、体积与速率并从官网零注册表解包全套便携环境后自动进入主控制台；选择“否”则提示“请把程序放在你需要部署的目录再次运行”并在当前目录零残留文件/文件夹的状态下安全退出。
- **Pre-boot 零污染隔离**：前置子窗口的 WebView2 临时缓存完全隔离于 `%TEMP%\AideRun_preboot_<pid>`，用户点击“否”退出前不在程序所在目录生成 `data/`、`projects/` 等任何目录，且退出或进入主面板时自动清理临时上下文。
- **静默进度下载器**：新增 `net::download_file_with_progress` 与 `net::probe_content_length`，GUI 宿主环境下自动抑制控制台窗口闪烁。
- **环境隔离加固**：`env::build_isolated_env` 补充注入 `HOMEDRIVE` 与 `HOMEPATH` 指向 `data/profile` 分区与相对目录，进一步杜绝部分 Node/Electron/CLI 工具回退探测系统 `%HOMEDRIVE%%HOMEPATH%`。

## [1.2.1] - 2026-11-12

### 优化与清理
- **遗留旧脚本与 Scoop 彻底退场（方案 A 完备收口）**：
  - 移除根目录旧版 `Start-Portable.vbs`、`Antigravity-Portable.bat`、`Manage-Tools.bat`、`Clean-Cache.bat`、`Clean-Account-For-Packaging.bat` 与 `tools/clean_account.py`，并清理历史遗留的 `tools/scoop` 目录。
  - 在 `paths::ensure_skeleton` 中同步预置 `data/profile/npm-global`，配合 `NPM_CONFIG_PREFIX` / `npm_config_prefix` 实现 Node.js 全局包的跨盘符纯绿隔离。
- **版本号展示位置调整**：
  - 移除窗口左上角标题旁的版本徽标，统一移动至程序窗口右下角状态栏清晰呈现（如 `AideRun v1.2.1 · IDE v2.6.0 · 右键托盘多开`）。
- **「Win检测升级」按钮激活条件精细化**：
  - 将「一键在线无痕升级」主按钮设为默认禁用；仅当**自动检测到官方 Win 版本号严格高于当前本地版本**，或**手动通过文件选择器选定本地安装包（`.exe` / `.zip`）**后才会激活该按钮。
  - 升级或秒级回滚完成后自动重算版本缓存并同步按钮禁用/激活状态，避免无意义重复触发。
- **「组件管理」新增新手开发场景一键预设**：
  - 新增 `全能开发推荐 (全部5项)`、`Python / AI (uv+Git)`、`前端全栈 (Node+Git)`、`基础装机 (解包+Git)` 四组快捷预设，自动跳过已就绪最新组件并按序安装/升级缺失项，实时刷新列表进度。

## [1.2.0] - 2026-11-12

### 稳定性修复与架构重构
- **防静默闪退保护体系**：
  - 将 Release 编译策略由 `panic = "abort"` 调整为可捕获恢复的 `unwind` 模式，并在程序入口注册全局 Panic 崩溃钩子（`data/cache/logs/panic.log` + Win32 异常详情弹窗），彻底消除无日志静默退出现象。
  - 修复清理引擎在控制面板运行期误删 `data/cache/webview2`（当前 `msedgewebview2.exe` 运行目录）的隐患：面板端扫描与清理强制跳过运行中 WebView2 数据目录。
  - 消除事件引擎与后台工作线程中的所有 `.lock().unwrap()` 及并发重入冲突，新增任务互斥护盾与 `catch_unwind` 线程隔离，拦截非预期新窗跳转。
- **「清理与打包」标签页重构**：
  - 全量改版为**实时垃圾组成明细表**（按空间降序展示各项名称、日常/打包归类、相对路径与文件占用），清晰掌握垃圾构成。
  - 整合操作为**两大按钮**：`一键清理日常垃圾（推荐）` 与 `打包之前彻底清理`（点击弹出轻量磨砂确认窗，可选是否同步执行账号与对话隐私脱敏，防止日常误删登录态）。
  - 新增并持久化 **`程序智能清理`** 复选项（写入 `data/launcher.toml`）：勾选后在 IDE 主进程退出后、或启动发现日常垃圾超过阈值时后台静默清理未锁定日常缓存。
- **「Win检测升级」标签页精简**：
  - 移除多余的手动「检测 Win 官方新版」按钮，改为**首次切换至本标签页时自动检测**，并在当次运行 `AideRun.exe` 生命周期内复用该单次检测结果，不重复发起网络请求。
  - 剔除版本号后缀冗余构建号显示，只保留清晰直观的本地/官方最新版本号对比。
- **「组件管理」综合平台重塑（彻底去除 Scoop）**：
  - 从面板 UI、体检报告与清理清单中彻底移除全部 Scoop 状态胶囊、按钮与文案。
  - 升级为原生零软链接**便携组件综合管理平台**：集中展示 `innounp-2`、`uv`、`7-Zip`、`MinGit`、`Node.js LTS` 及自定义组件的**安装状态**、**实时本地版本号**、**官方最新版本**与**升级状态（已是最新 / 可升级 / 待安装）**，支持一键安装、升级、卸载与可视化添加自定义组件。

## [1.1.0] - 2026-11-12

### 新增与升级
- **水晶磨砂玻璃拟态启动面板 (`860×560` + 系统托盘守护)**：
  - 基于原生 `tao + wry (Edge WebView2) + tray-icon` 构建轻量零外部依赖启动台；双击 `AideRun.exe` 默认打开紧凑半透明大圆角控制面板，点击启动 IDE 后自动最小化至系统托盘（右键托盘菜单支持快速启动、项目多开、便携 PowerShell、检测 Win 更新与退出）；保留 `--silent` 直启及拖放文件夹直启。
  - 实现 50ms 后台驱动的 HSL 360° 七彩呼吸极光引擎（步长 0.35°、饱和度 52%、亮度 85%），严格遵循 20px 外圆角、#0DFFFFFF 磨砂卡片与 #1A252F 高对比深冷灰字体。
  - WebView2 运行缓存强制重定向隔离至 `data/cache/webview2`，绝不污染宿主机系统盘 `AppData`，并纳入深度清理项。
- **官方品牌图标全链路嵌入**：
  - 拉取官方 `antigravity-icon__full-color.svg`，生成多分辨率透明 `app.ico`（16~256px）及高清 RGBA 图标，通过 `build.rs` 与 `embed-resource` 无缝应用到 EXE 文件图标、窗口/任务栏图标、系统托盘图标、桌面快捷方式及面板顶部 Brand Logo。
- **并行多开与分身隔离引擎**：
  - 支持一键扫描 `projects/` 目录并通过 `--new-window` 同配置并行开启多个项目窗口；同时支持原生地创建与拉起 `data/instances/<分身名>/` 完全隔离的多账号/多配置 IDE 实例（可选共享扩展库）。
- **升级解包器切换为 `innounp-2` & 仅限 Win 平台检测**：
  - `check-update` 严格核验 `windows-x64/Antigravity%20IDE.exe` 官方分发链路可达性，屏蔽非 Windows 平台干扰条目。
  - 安装包零注册表解包引擎由 `innoextract 1.9` 切换为 `innounp-2`（原生支持 Inno Setup 6.4+ 最新安装包结构，保留 `innoextract` 与 `7zip` 作为兼容备选）。

## [1.0.0] - 2026-11-12

### 新增功能
- **极简单文件启动器 (`AideRun.exe`)**：基于 Rust 原生静态编译（启用 `+crt-static`），单文件运行无外部运行库依赖，统一替代原 `Start-Portable.vbs`、4 个 `.bat` 工具脚本与 `tools/clean_account.py`。
- **空目录自动部署便携环境 (`bootstrap`)**：当检测到目录中尚未建立便携结构（例如只有 `AideRun.exe` 与 `Antigravity IDE.exe`）时，自动构建全套 `data/`、`projects/`、`tools/` 隔离结构与默认 `zh-cn` 便携配置；若检测到 `Antigravity IDE.exe` 为官方单文件 Inno Setup 安装包，支持直接无痕解包就绪（零注册表）。
- **静默启动与全隔离环境注入**：双击无黑框闪屏启动；完整接管 `USERPROFILE`、`HOME`、`APPDATA`、`LOCALAPPDATA`、`TEMP`、`TMP`、`CARGO_HOME`、`RUSTUP_HOME`、`PIP_CACHE_DIR`、`UV_CACHE_DIR`、`NPM_CONFIG_CACHE`、`GOPATH`、`GOCACHE` 等路径；默认追加 `--disable-gpu-disk-cache`；默认不在宿主机桌面生成残留快捷方式（按需通过 `shortcut` 命令创建）。
- **只读系统代理同步**：只读探测 Windows 代理设置，使用标准 JSON 解析同步更新 `config.json`，不写注册表、不破坏原有配置键。
- **官方新版检测与无痕升级 (`check-update` / `upgrade`)**：支持从 `https://antigravity.google/download`、`https://antigravity.google/releases` 及官方发布接口自动检测最新版本；支持对官方单文件安装包进行零注册表解包与内核原子替换，严格保护 `data/`、`projects/`、`tools/`、`version.dll`、`config.json` 与 `AideRun.exe`，支持快照回滚 (`--rollback`)。
- **双模便携工具链 (`tools`)**：自动修复 `tools/scoop` 跨盘符移动导致的 Junction 与 `.shim` 绝对路径失效（在 exFAT/FAT32 非 NTFS 磁盘上自动降级直挂版本号目录）；同时支持零软链接原生绿色便携组件下载与自定义配置。
- **智能进程互斥清理与原生 SQLite 脱敏 (`clean` / `sanitize`)**：精准检测当前便携目录进程占用；支持 `light`、`deep`、`all` 分级清理与 `--dry-run` 体积统计；内置 SQLite 引擎原生脱敏打包，彻底清理账号凭据、对话历史、通知冗余并压缩数据库，完整保留简体中文与免权限设置 (`agentPreferences`)。

