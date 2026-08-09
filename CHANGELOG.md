# Changelog

本文件记录 SteamTools 每个发布版本的更新内容。
格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/), 版本号遵循语义化版本 (SemVer)。

## [Unreleased]

## [v0.2.1] - 2026-08-09

社区源 ticket 贯通 (lua / 注册表), 以及库「购买」相关的 package0 / forge 闸门修复。

### Added

- **Catalog ticket 字段**: `CatalogBundle` 保留 `app_tickets` / `etickets` / `steam_ids`,
  社区源解析不再丢弃 ticket 相关字段
- **lua `setAppticket`**: DSL 写出 app ticket; 导入兼容 `UserGameStatsSchema`
- **HKCU 凭据**: 平台层读写 `AppTicket` / `ETicket` / `SteamID`, 与入库落盘对齐

### Fixed

- **热重载误删 package0**: lua 重载 / 导入后改用 `ensure_configured` 只补缺,
  禁止 `reconcile_owned` 把仍受管 id 清掉 (库变「购买」)
- **冷启 forge 闸门**: seed / 同步 configured 后刷新 `CONFIGURED_NONEMPTY`,
  避免冷启动 CheckAppOwnership 不 forge、库仍显示购买

## [v0.2.0] - 2026-08-03

入库链路补齐 DLC / 清单下载数据, 配置面板支持拖放导入社区 lua, 下载与 package 注入若干稳定性修复。

### Added

- **自动 DLC**: 主游戏入库后可按配置自动扩展 DLC; 有清单+密钥则完整合并,
  否则仅 `addappid` 解锁; 失败/超时不拖垮主游戏 (`catalog.auto_dlc`, 默认开)
- **商店 DLC 勾选**: 入库菜单增加「仅游戏 / 选择 DLC」; Store DLC picker 浮层,
  `CatalogDlcMode` (Full / GameOnly / Selected) 两段 list/commit 协议
- **本地 lua 拖放导入**: 配置面板「脚本目录」支持 dropzone / 文件选择;
  `import_local` 引擎校验 DSL 后写入 `config/lua`, 即时 package/库对齐;
  工具开关 `lua_drop` (拖放导入)
- **社区 DSL 大小写别名**: 兼容 `setManifestid` / `AddAppId` / `AddToken` /
  `setAppDepots` 等社区脚本常见写法
- **Catalog 源链增强**: CatMisteam 完整 Provider + key Enricher;
  Community → CatMisteam → CaiGamer 回退; token 缺失时 CaiGames appinfo 补
  access token; `listofdlc` 等字段解析带出 `related_dlc_ids`
- **入库缺下载数据提示**: 成功但缺 depot key / access token 时商店页弹窗提示
  (可右键刷新清单重试); 入库结果携带 `MissingDownloadData`
- **depotcache 落盘**: manifest 双写 Steam `depotcache`; `setmanifestid` 写出
  非零 size, 避免假 license 安装对话框显示 0B
- **package0 multi-app 注入**: 入库后覆盖主 app + 全部 depot (对齐 OST);
  库 UI 仍只认主 app; 反馈 DLC 计数 (`dlc_unlock` / `dlc_full`)
- **配置面板**: 源设置页「自动添加 DLC」开关; 脚本页底部粘性反馈条

### Changed

- Community 成功路径不再因 key 不完整硬失败; 缺 key 改由上层 missing 提示
- 管理列表识别 `stt_` / `import_` / 纯数字文件名

### Fixed

- **package0 自愈**: Steam 原生卸载清空 AppIdVec 后, 其它入库游戏一起从库消失;
  周期 `heal_sync` + notify 前 resync, 不依赖用户点「刷新清单」
- **无 key depot 剪枝**: 缺密钥的 DLC 仓不再进入下载面, 避免整包被判加密
- **depot key hook**: 去掉错误的 EConfigStore UserLocal 过滤; 路径匹配对齐 OST
  (`\DecryptionKey` / `/`)
- **request-code**: 多源 HTTP 回退链 (opensteamtool / wudrm / steamrun);
  recv 最多等 12s; 解析允许首尾空白
- **共享 send hook**: token / request-code 共用 `BBuildAndAsyncSendFrame`,
  已挂 hook 只激活 consumer; 去掉硬编码 steamclient SHA
- **库 UX**: MarkAppChange detour 捕获 this; 移除 drain 后通知 UI;
  CDP pipe/ws 分类日志与 reply 总时限
- **安装器**: 固定安装到已验证的 Steam 根目录 (去掉目录选择页);
  多源探测注册表与常见路径, 要求存在 `steam.exe`

## [v0.1.0] - 2026-08-01

首个公测版 (M0–M8 + 发布形态): Steam 内嵌工具箱, 纯 Rust, DLL 劫持加载。

### Added

- **进程进入**: 纯 Rust 双加载器 (`dwmapi.dll` / `xinput1_4.dll`) 劫持加载
  `stbase.dll` (host), 会话追加日志 `steamtools/host.log` (4 MiB 轮转 + 脱敏)
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
- **下载清单 (download_kit)**: 为 Steam 下载提供所需数据 — manifest 绑定 (GID/size)、
  depot key、access token、manifest request code (不加速下载, 也不代下载); 均按
  exact SHA + 入口签名门禁可降级 attach, 独立 feature + 运行时开关
- **商店加速 (store_accel)**: DLL 内代理线程, PAC + HTTP CONNECT, 本地 DNS/CDN
  优选 (A/AAAA 缓存 + 候选测速 + 健康冷却), 可选 Clash loopback 回退;
  不装根证书 / 不做 MITM / 不写 Hosts / 不加载 WinDivert, 启停回滚 PAC
- **版本兼容**: 每启动按 SHA-256 匹配 `SteamTools-Patterns` 远端 pattern
  (原子缓存), 远端不可用回退内置样本与 legacy 缓存; 缺 pattern 只禁用该能力
- **自动更新**: 启动时检查 GitHub `/releases/latest` (302 Location 取 tag), 下载
  `checksums.sha256` + 宿主, SHA-256 钉定校验后 rename-swap 进 Steam 根目录,
  重启生效; 崩溃自愈 (`.old` 回滚); `[update]` 可关; 配置页显示更新状态
- **一键安装器**: `SteamTools-Setup-<ver>.exe` (Inno Setup) — 自动检测 Steam 根目录
  (注册表), Steam 运行检测, 按需提权; 卸载器收敛在 `steamtools/` 数据目录,
  卸载时删除文件与数据目录
- **CI 与发布工程**: GitHub Actions CI (fmt / clippy / 串行测试 / release 构建 /
  UI 产物检查); 一键发包脚本 + Release CI 自动发布; Release 资产含
  `checksums.sha256` 供自更新校验

### Changed

- 许可证改为 GPL-3.0-or-later; `Cargo.lock` 入库保证可复现构建
- `store_accel` 从独立 exe 内嵌为 `stbase.dll` 内线程 (无独立 helper)
- 配置面板从 Rust 手写 DOM 迁移为 Preact + TSX 组件化 (M6.10)
- 移除实验代码: native CEF ExecuteJavaScript 注入路径 (CDP 为主路径)
- 宿主产物名 `SteamTools.dll` → `stbase.dll` (模块名中性化, 降低进程枚举指纹);
  发布脚本同步修正

### Fixed

- CEF hook 首拉错过导致入口丢失: hook 安装提前到 init 最前 (`6b4fd95`)
- 商店 -118 静态资源失败边界: 有限 Clash 静态回退 + 同域候选重试
- 发布管线产物名 stale: release 脚本仍拷已不存在的 `SteamTools.dll`
- 安装器静默安装目录为空 (`GetSteamDir` 读未初始化页面值)

### Security

- 日志脱敏: 不记录 key / token / ticket / cookie / 查询参数
- store_accel allowlist 外请求不经过 helper; 不安装根证书
- 自更新校验: 资产 SHA-256 钉定, 拒绝清单缺失/不匹配的下载
