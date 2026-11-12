<!-- AideRun - Antigravity IDE 便携版一体化启动器与全生命周期管理控制台项目说明文档 -->

# AideRun

**AideRun** 是一款专为 Windows 平台 **Antigravity IDE** 打造的零污染、全便携桌面端启动器与环境管理控制台，基于 **Rust + tao + wry (WebView2) + tray-icon** 构建，主打**开箱即用、彻底环境隔离、零注册表官网一键部署**与 **Crystal Glassmorphism（水晶玻璃拟态）** 交互体验。

> 当前最新版本：**v1.3.0** ｜ 编译成品已归档于 [`Release/AideRun.exe`](Release/AideRun.exe)

---

## 核心特性

### 1. 启动前置双引导子窗口（v1.3.0）
双击启动 `AideRun.exe` 时，程序会先在**不向当前目录写入任何文件/文件夹**的前提下完成两道静默前置检测：
- **网络代理与 Google 连通性自检（`480 × 410`）**
  - 自动并发探测常见 Windows 代理软件端口（Clash `7890`/`7897`、v2rayN `10809`/`10808`、Shadowsocks `1080`、NekoBox `2080`、Hiddify 等）、Windows 系统代理、环境变量代理及 TUN 直连状态。
  - 若可正常访问 Google，静默跳过；若检测不通过，自动弹出代理配置子窗口，支持常用软件端口快捷填入、粘贴完整代理 URL 并实时测试连通性。
- **便携 IDE 环境检测与官网一键部署（`500 × 360`）**
  - 若当前目录已包含 Antigravity IDE 环境（或待解包的官方安装包），直接进入主控制台。
  - 若目录中仅存在启动器本体，弹窗询问：`当前目录没有 Antigravity IDE 环境，是否开始现在部署 Antigravity IDE 便携开发环境？`
    - 选择 **“是，立即下载部署”**：自动从 `antigravity.google` 获取最新 Windows x64 安装包并在子窗口实时展示下载进度、速率与日志，调用便携 `innounp` 完成**零注册表解包部署**。
    - 选择 **“否”**：提示 `请把程序放在你需要部署的目录再次运行` 并在当前目录**零残留**安全退出。

### 2. 彻底的跨盘符便携隔离
所有用户数据、扩展插件、AI 会话档案与开发缓存统一收口到软件根目录下的 `data/` 中，**绝不污染宿主系统 C 盘**，支持整包打包迁移或拷贝至 U 盘随身使用：
- `data/user-data/` & `data/extensions/`：VS Code / IDE 用户配置与插件目录
- `data/profile/`：完整隔离的个人用户根目录（注入 `USERPROFILE`、`HOME`、`HOMEDRIVE`、`HOMEPATH`、`APPDATA`、`LOCALAPPDATA`、`CARGO_HOME`、`RUSTUP_HOME`、`DOTNET_CLI_HOME` 等），确保 `.gemini` 助手数据与开发者工具链全部留在便携目录
- `data/cache/`：统一承接系统临时目录与包管理器缓存（`Temp`、`npm`、`pip`、`uv`、`cargo-target`、`webview2` 等）
- `projects/` & `tools/portable/`：默认工作空间与绿色工具链容器

### 3. 主控制台与全生命周期运维
- **水晶玻璃拟态 860×560 主控面板**：内置 HSL 360° 呼吸流光引擎（50ms 刷新、0.35° 步长），支持系统托盘常驻、多开独立隔离实例、一键创建桌面快捷方式与便携 PowerShell 终端。
- **智能安全清理与防误杀**：基于 SQLite 清理、分类磁盘扫描、进程占用保护与退出后自动日常瘦身。
- **便携工具链管理 & IDE 在线升级**：内置原生绿色组件安装/状态检测及原子替换内核升级、异常回滚机制。

---

## 目录结构

```text
AideRun/
├── .cargo/config.toml        # Cargo 编译配置（启用 +crt-static 与统一 target-dir）
├── Release/
│   └── AideRun.exe           # 最新 Release 静态编译可执行成品（供分发与无感升级）
├── assets/                   # WebView2 前端模板（panel.html / preboot.html）与图标资源
├── src/
│   ├── main.rs               # 程序入口、控制台抑制与子命令路由
│   ├── gui.rs                # 860×560 主控制台、系统托盘与全局事件编排
│   ├── preboot_gui.rs        # 启动前置代理检测与便携部署双引导子窗口控制器
│   ├── bootstrap.rs          # 便携骨架部署、环境诊断与官方安装包解包部署引擎
│   ├── proxy.rs              # 常见代理端口扫描、Google 连通验证与 config.json 同步
│   ├── net.rs                # 网络请求、静默实时进度下载与压缩包解压
│   ├── env.rs                # 便携环境变量隔离注入（含 HOMEDRIVE/HOMEPATH）与 PATH 构造
│   ├── upgrade.rs            # 官方最新版本检查、零注册表解包与原子内核替换升级
│   ├── toolbox.rs            # 内置便携开发工具链管理
│   ├── clean.rs / sanitize.rs# 缓存清理扫描与敏感状态脱敏
│   ├── launch.rs / process.rs# IDE 主窗口/多开实例拉起与进程锁检测
│   ├── config.rs / paths.rs  # 启动器配置读写与便携路径解析
│   └── ...
├── build.rs                  # Windows 应用程序清单与图标资源嵌入
├── Cargo.toml                # Rust 工程配置与依赖声明
├── Cargo.lock                # 锁定依赖版本
└── CHANGELOG.md              # 简体中文版本更新日志
```

---

## 快速使用

### 直接运行预编译成品
1. 下载 [`Release/AideRun.exe`](Release/AideRun.exe)，放入任意希望部署 Antigravity IDE 的空目录（或已有 Antigravity IDE 目录）。
2. 确保本地代理软件已开启（或处于可访问 Google 的网络环境），双击运行 `AideRun.exe`。
3. 跟随引导完成网络检测与官网自动便携部署后，即可进入主控制面板使用。

### 从源码编译

#### 环境要求
- Windows 10 / 11 (x86_64)
- Rust 工具链（`stable-x86_64-pc-windows-msvc`）与 MSVC Build Tools
- Microsoft Edge WebView2 Runtime（Windows 11 默认内置）

#### 构建与测试命令
```powershell
# 运行全部单元测试
cargo test

# 编译 Release 可执行文件
cargo build --release
```

> 提示：项目内 [`.cargo/config.toml`](.cargo/config.toml) 默认配置了 `target-dir = "../../data/cache/cargo-target"` 以适配便携一体化开发环境；若作为独立源码仓库单独克隆编译，可按需修改或删除该配置项以输出到默认 `target/` 目录。

---

## 更新日志

详细变更记录请参阅 [CHANGELOG.md](CHANGELOG.md)。

