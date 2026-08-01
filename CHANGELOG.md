# Changelog

本文件记录 SteamTools 每个发布版本的更新内容。
格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/), 版本号遵循语义化版本 (SemVer)。

## [Unreleased]

### Added

- 发布工程: `tools/release.ps1` 一键发包 + Release CI 自动读取 CHANGELOG 并创建 GitHub Release

## [v0.1.0] - 2026-08-01

首个里程碑完整版 (M0–M8): Steam 内嵌工具箱, 纯 Rust, DLL 劫持加载。

### Added

- **进程进入**: 纯 Rust 双加载器 (`dwmapi.dll` / `xinput1_4.dll`) 劫持加载
  `SteamTools.dll` (host), 会话追加日志 `steamtools/host.log` (4 MiB 轮转 + 脱敏)
- **配置**: `steamtools.toml` 宿主配置 + `config/lua` 生态兼容 (mlua), 热重载,
  原子落盘
- **商店入库 (catalog_add)**: 商店详情页「入库」按钮 (CDP 注入),
  Catalog provider chain (CustomHttp / Lua / 社区多源) + 集中校验,
  元数据原子落盘 `config/lua/stt_{id}.lua`
- **配置面板 (config_ui)**: Preact + TSX 单文件内嵌包 (~38 KB), 工具开关 /
  Catalog 源 / 日志级别 / Lua 目录, Esc / 遮罩 / 按钮三条关闭路径
- **库 UX (library_ux)**: 受管 App 右键追加「刷新清单 / 移除入库」(保留 Steam 原菜单);
  M4a 真实 steamui detour (RunFrame / FillInAppOverview / BuildCompleteAppOverviewChange),
  移除队列 drain 清 ownership + MarkAppChange, removed_appid 重注入
- **下载套件 (download_kit)**: 四项独立能力 — manifest 绑定 (GID/size)、
  depot key、access token、manifest request code; 均按 exact SHA + 入口签名
  门禁可降级 attach, 独立 feature + 运行时开关
- **商店加速 (store_accel)**: DLL 内代理线程, PAC + HTTP CONNECT, 本地 DNS/CDN
  优选 (A/AAAA 缓存 + 候选测速 + 健康冷却), 可选 Clash loopback 回退;
  不装根证书 / 不做 MITM / 不写 Hosts / 不加载 WinDivert, 启停回滚 PAC
- **版本兼容**: 每启动按 SHA-256 匹配 `SteamTools-Patterns` 远端 pattern
  (原子缓存), 远端不可用回退内置样本与 legacy 缓存; 缺 pattern 只禁用该能力
- **CI 与发布工程**: GitHub Actions CI (fmt / clippy / 串行测试 / release 构建 /
  UI 产物检查); 一键发包脚本 + Release CI 自动发布

### Changed

- 许可证改为 GPL-3.0-or-later; `Cargo.lock` 入库保证可复现构建
- `store_accel` 从独立 exe 内嵌为 `SteamTools.dll` 内线程 (无独立 helper)
- 配置面板从 Rust 手写 DOM 迁移为 Preact + TSX 组件化 (M6.10)
- 移除实验代码: native CEF ExecuteJavaScript 注入路径 (CDP 为主路径)

### Fixed

- CEF hook 首拉错过导致入口丢失: hook 安装提前到 init 最前 (`6b4fd95`)
- 商店 -118 静态资源失败边界: 有限 Clash 静态回退 + 同域候选重试

### Security

- 日志脱敏: 不记录 key / token / ticket / cookie / 查询参数
- store_accel allowlist 外请求不经过 helper; 不安装根证书

## [test] - doc-only change
