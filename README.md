<!-- AideRun - Antigravity IDE 便携版全能启动器项目说明文档 -->
<div align="center">

<img src="https://raw.githubusercontent.com/ZhangSir9901/AideRun/main/assets/icon_256.png" width="100" alt="AideRun Logo" onerror="this.src='https://raw.githubusercontent.com/ZhangSir9901/AideRun/main/assets/logo.svg'">

<h1>AideRun · Antigravity IDE 便携启动器</h1>

**专为极致便携与高洁癖开发者打造的一体化 Antigravity IDE 零污染桌面启动器与全生命周期管家**

集 Antigravity IDE 自动下载、便携部署、日常清理、全组件管理于一体的工具。致力于摆脱系统盘路径污染与复杂环境配置，原生提供启动前智能代理检测、官方原版零注册表一键部署、跨盘符即插即用自愈、自身无感影子热升级、多开分身隔离以及水晶玻璃拟态控制台体验。

<p align="center">
  <img src="https://img.shields.io/github/stars/ZhangSir9901/AideRun?style=flat-square&color=3498db&logo=github" alt="stars">
  <img src="https://img.shields.io/badge/version-v1.3.2-1abc9c?style=flat-square" alt="version">
  <img src="https://img.shields.io/github/issues/ZhangSir9901/AideRun?style=flat-square&color=f1c40f" alt="issues">
  <img src="https://img.shields.io/badge/arch-x86__64-lightgrey?style=flat-square" alt="arch">
  <img src="https://img.shields.io/badge/OS-Win10_11-success?style=flat-square" alt="os">
  <img src="https://img.shields.io/badge/Built_with-Rust-orange?style=flat-square&logo=rust&logoColor=white" alt="Rust">
  <a href="https://t.me/YuC2027" target="_blank"><img src="https://img.shields.io/badge/Telegram-@YuC2027-2CA5E0?style=flat-square&logo=telegram&logoColor=white" alt="Telegram"></a>
</p>

<h3>
  <a href="#️-使用说明">快速开始</a> · 
  <a href="#-核心功能一览">核心特性</a> · 
  <a href="#-目录结构与源码编译">源码编译</a> · 
  <a href="#-更新日志">更新日志</a> · 
  <a href="#-意见反馈与技术支持">技术支持</a>
</h3>

<br>

</div>

> 💡 **最新版直链下载**：[Release/AideRun.exe](https://raw.githubusercontent.com/ZhangSir9901/AideRun/main/Release/AideRun.exe)（单文件静态编译成品，放入目标目录双击即可自动部署全套便携开发环境）

---

## ✨ 核心功能一览

### 🌐 1. 启动前置双引导：代理检测与官网零注册表一键部署
*   **智能代理自检与单行多格式识别**：启动 GUI 时无副作用并发探测常见 Windows 代理软件端口（Clash `7890`/`7897`、v2rayN `10809`/`10808`、Shadowsocks `1080`、NekoBox `2080`、Hiddify 等）、系统代理、环境变量与 TUN 直连 Google 连通性；未连通时自动弹出与主面板同尺寸（`1120 × 700`）的水晶玻璃拟态代理配置子窗口，支持常用端口快填、**单行代理字符串智能解析**（自动识别服务器、端口、HTTP/SOCKS5 协议、用户名与密码，覆盖 URL、带账密 `ip:port:user:pass` 及键值对片段，无协议头时自动双协议回退测通），以及点击「自动检测当前网络」一键探活回填。
*   **纯净目录自动部署**：若目录中仅存在 `AideRun.exe`（缺少 IDE 环境），确认后直连官网获取最新 Windows x64 安装包并在同尺寸（`1120 × 700`）子窗口实时展示下载进度与速率，调用便携 `innounp` 完成**零注册表原地解包**；若点击“否”则提示并在当前目录**零残留**安全退出。

### 🚀 2. 自身无感热升级：GitHub 检测与影子热替换
*   **静默版本探测**：主面板每次启动自动对比远端 [`Release/update.json`](https://github.com/ZhangSir9901/AideRun/blob/main/Release/update.json)（兼容直连代理与镜像回退），发现新版立即在底部状态栏以醒目的**呼吸提醒按钮**弹出通知，亦可在「检测与升级」页手动触发检测。
*   **防锁死影子热替换**：下载新版至 `.new` 并校验 Windows PE 文件头合法性，随后原子执行 `.old` 重命名与新版上位替换，自动释放托盘后通过 `--wait-pid` 等待旧进程句柄销毁并秒级重启新版，旧残留自动静默清除。

### 🛡️ 3. 彻底跨盘符隔离：告别 C 盘膨胀
*   **全路径环境变量接管**：自动注入隔离版 `USERPROFILE`、`HOME`、`HOMEDRIVE`、`HOMEPATH`、`APPDATA`、`LOCALAPPDATA`、`CARGO_HOME`、`RUSTUP_HOME`、`NPM_CONFIG_PREFIX` 等，将 Antigravity 用户数据、插件、`agy` 语言后端、Cargo/Rust/Python/npm 缓存全部收口至同级 `data/` 目录。
*   **随身携带免配置与跨盘符零感知自愈**：支持 U 盘部署或整包拷贝跨盘符即插即用，系统 `PATH` 与桌面保持绝对纯净；更换电脑导致 U 盘盘符变化时，启动器自动完成扩展索引（`extensions.json`）、最近工作区（`storage.json` / `state.vscdb`）及 `.gitconfig` 等路径的无感重定位（带快照回滚保护）。

### ✨ 4. 水晶玻璃拟态面板与托盘多开分身
*   **工业级美学 UI**：基于 **Rust + tao + wry (WebView2)** 原生渲染 `1120 × 700` 无边框透明窗口（主界面与前置子窗口统一尺度），搭载 50ms 驱动 HSL 360° 呼吸流光引擎（步长 `0.35°`、S `0.52`、L `0.85`），主窗口与各个子窗口顶部标题栏及右下角均内置 GitHub 仓库一键直达按钮。
*   **工作区隔离多开**：一键系统托盘常驻、按项目文件夹独立多开、或通过 `--instance <名称>` 创建完全隔离的数据分身，并内置便携 PowerShell / CMD 终端启动器。

### 🧹 5. 智能清理、脱敏与内核升级护盾
*   **分级瘦身与进程保护**：智能识别 IDE 锁占用，提供一键日常清理与打包前彻底瘦身（支持保留配置仅清除缓存，或一键 SQLite 隐私脱敏分享给他人）。
*   **IDE 内核零注册表升级 & 回滚**：一键从官方通道升级 Antigravity IDE 本体，自动保护 `data/`、`projects/`、`tools/` 与 `config.json` 并保留可秒级回滚快照。

---

## 🛠️ 使用说明

1. **零基础全新部署**：下载 [`Release/AideRun.exe`](https://raw.githubusercontent.com/ZhangSir9901/AideRun/main/Release/AideRun.exe) 放入希望安放 Antigravity IDE 的文件夹并双击运行，按引导完成代理验证与“是，立即下载部署”即可自动装好全部环境。
2. **已有目录直接接管**：将 `AideRun.exe` 放入已解包的便携版 Antigravity IDE 根目录，双击直接秒进 1120×700 控制面板。
3. **静默直启与命令行**：
   - `AideRun.exe --silent` 或将项目文件夹拖放到图标上：跳过控制面板，注入完整便携环境直启 IDE。
   - `AideRun.exe doctor` / `clean` / `tools list`：支持完整命令行体检、清理与原生零软链接组件管理。

---

## 💻 目录结构与源码编译

```text
AideRun/
├── Release/
│   ├── AideRun.exe           # 最新静态编译成品（供分发与无感自升级）
│   └── update.json           # 远端自升级版本清单
├── assets/                   # WebView2 前端模板（panel.html / preboot.html）与图标
├── src/
│   ├── main.rs               # 入口路由、控制台抑制与 --wait-pid 自升级重启衔接
│   ├── gui.rs                # 主控制台、系统托盘与全局状态事件调度
│   ├── preboot_gui.rs        # 启动前代理检测与官网一键部署双引导子窗口
│   ├── self_update.rs        # AideRun 本体 GitHub 版本探测与影子热替换自升级
│   ├── bootstrap.rs          # 便携骨架搭建、环境诊断与官方安装包解包引擎
│   ├── proxy.rs              # 常见代理端口并发扫描、Google 校验与配置同步
│   ├── env.rs                # 彻底便携环境变量注入（含 HOMEDRIVE / HOMEPATH）
│   ├── upgrade.rs            # Antigravity IDE 内核检测升级与异常回滚
│   ├── clean.rs / sanitize.rs# 垃圾缓存统计清理与 SQLite 隐私脱敏
│   └── ...
├── .cargo/config.toml        # Cargo 编译目标重定向与静态 CRT 配置
└── CHANGELOG.md              # 简体中文版本更新日志
```

### 本地编译要求与命令
- **环境要求**：Windows 10 / 11 (`x86_64`)、Rust `stable-x86_64-pc-windows-msvc` 工具链、MSVC Build Tools、Microsoft Edge WebView2 Runtime。
```powershell
# 运行全部单元测试
cargo test

# 编译 Release 成品
cargo build --release
```

---

## 📋 更新日志

完整版本迭代历史请查看 👉 [CHANGELOG.md](https://github.com/ZhangSir9901/AideRun/blob/main/CHANGELOG.md)

---

## 💬 意见反馈与技术支持

软件在持续优化迭代中，如果您在使用过程中遇到了任何问题、发现 Bug 或有更好的功能建议，**非常欢迎与我联系或提交反馈！** 您的每一个建议都是本工具不断完善的最大动力。

*   <img src="https://cdn.simpleicons.org/wechat/07C160" width="16" align="center" /> **微信咨询**：`jiujiujiayi666` (点击程序界面上的微信图标可直接复制)
*   <img src="https://cdn.simpleicons.org/telegram/26A5E4" width="16" align="center" /> **Telegram**：[@YuC2027](https://t.me/YuC2027)
*   <img src="https://cdn.simpleicons.org/googlechrome/4285F4" width="16" align="center" /> **官方网站**：[乾旭网络](https://www.qianxu.vip)
*   <img src="https://cdn.simpleicons.org/github/181717" width="16" align="center" /> **开源仓库**：[ZhangSir9901/AideRun](https://github.com/ZhangSir9901/AideRun)
*   <img src="https://cdn.simpleicons.org/github/181717" width="16" align="center" /> **提交 Issue**：欢迎在 GitHub 提交 [Issues](https://github.com/ZhangSir9901/AideRun/issues) 交流探讨。

---

## 👥 贡献者 (Contributors)

感谢所有参与反馈和探讨的朋友们！

<a href="https://github.com/ZhangSir9901/AideRun/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=ZhangSir9901/AideRun" />
</a>

## 📈 关注曲线 (Star History)

[![Star History Chart](https://api.star-history.com/svg?repos=ZhangSir9901/AideRun&type=Date)](https://star-history.com/#ZhangSir9901/AideRun&Date)

<br>
<div align="center">
  <i>"Talk is cheap. Show me the code." —— 保持热爱，奔赴山海。欢迎大家下载体验并提出宝贵意见！</i>
</div>
