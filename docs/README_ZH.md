![SteamTools](https://socialify.git.ci/SteamToolsProject/SteamTools/image?description=1&descriptionEditable=An%20in-process%20toolbox%20for%20the%20Steam%20client%20on%20Windows%2C%20written%20in%20Rust.&font=Inter&forks=1&issues=1&logo=https%3A%2F%2Fraw.githubusercontent.com%2FSteamToolsProject%2FSteamTools%2Fmaster%2Fassets%2Flogo.svg&name=1&owner=1&pattern=Diagonal%20Stripes&stargazers=1&theme=Dark)

<div align="center">

[![License](https://img.shields.io/badge/License-GPLv3-blue?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-DEA584?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Windows](https://img.shields.io/badge/Windows-10%2B-0078d6?style=flat-square&logo=windows&logoColor=white)](https://www.microsoft.com/windows)

[English](../README.md) · **简体中文**

**[快速开始](#使用)** · **[配置](#配置可选)** · **[构建](#构建)** · **[SECURITY](SECURITY.md)**

</div>

> [!Warning]
> 本工具仅用于**工程 / 研究练习**，请遵守当地法律与 Steam 服务条款，**测试请使用小号**。

---

## 关于

**SteamTools** 是 Windows 下 Steam 进程内的 Rust 工具箱：商店一键入库、库右键增强、内嵌配置面板、下载套件与商店加速。

主路径：商店详情页点「入库」→ 上游 Catalog 链拉元数据 → 原子落盘 `config/lua/stt_{id}.lua` → 库内可刷新 / 移除。
参考 [OpenSteamTool](https://github.com/OpenSteam001/OpenSteamTool) 的能力与 Lua 生态，产品形态与实现均为独立设计。

## 功能

### 入库 / 清单 `catalog_add`（默认开）

- 商店详情页「入库」按钮：AppId → 上游 Catalog 链（CustomHttp / Lua / 社区多源聚合）→ 集中校验 → 元数据**原子落盘** `config/lua/stt_{id}.lua`
- 失败可重试 / 降级：按钮文案、配置页 note、host.log 三处反馈口径一致
- 库内管理：受管 App 可一键刷新清单 / 移除入库

### 配置页 `config_ui`（默认开）

- Steam 内嵌配置面板（Preact + TSX，单 IIFE 38 KB，无运行时外部资源）
- 工具开关 / Catalog 源 / Manifest 源 / 日志级别 / Lua 目录，改动写 `steamtools.toml` 即热生效
- 支持 Esc / 遮罩点击 / 关闭按钮三条关闭路径，焦点不穿透到 Steam 背景

### 库 UX `library_ux`（默认开）

- 受管 App 右键追加「刷新清单 / 移除入库」，**Steam 原菜单完整保留**，操作只追加在底部
- 非受管 App 一律不拦，Steam 行为不变

### 下载套件 `download_kit`（默认开）

- 四项独立能力：manifest 固定（GID/size）、depot key、access token、manifest request code
- 全部**精确 SHA + 入口签名匹配**才挂载，缺符号 / 缺 pattern 时对应能力单独降级，其余不受影响
- 每项有独立运行时开关与工具中心状态（Ready / DataMissing / PatternMissing / EnvDisabled）

### 商店加速 `store_accel`（默认关）

- 本地 DNS/CDN 候选优选：UDP 公共 DNS、A/AAAA 独立缓存、TCP + SNI/证书/HEAD 探测、候选失败冷却与健康缓存
- 可选用户本机 Clash loopback 回退（本地候选失败最多回退一次）；`http_connect` 保留为自有中继可选出口
- PAC 保存与回滚；**不装根证书、不 HTTPS MITM、不写 Hosts、不加载 WinDivert**；`egress = "disabled"` 时完全不动系统代理

## 使用

1. 构建（前置：Windows 10+、Rust stable MSVC 工具链）：

   ```powershell
   cargo test --workspace --all-features --no-fail-fast -- --test-threads=1
   cargo build -p stt-host -p stt-store-accel -p stt-loader-dwmapi -p stt-loader-xinput --release
   ```

2. 把 3 个产物复制到 **Steam 根目录**（如 `C:\Program Files (x86)\Steam\`）：

   | 产物 | 说明 |
   |------|------|
   | `SteamTools.dll` | 宿主：配置 / 状态 / 全部能力（含内嵌商店加速线程，无需独立 helper） |
   | `dwmapi.dll` | 加载器（纯 Rust 转发 + 加载宿主） |
   | `xinput1_4.dll` | 加载器备选通道（同上） |

3. 重启 Steam，检查 `steamtools/host.log` 出现 `status=init complete`；商店详情页出现「入库」按钮，导航栏出现 SteamTools 入口。

> [!NOTE]
> 本地 WinHTTP 测试已知偶发 flaky，复跑即过，不影响构建结果。

## 配置（可选）

`steamtools.toml` 放在 Steam 根目录（与 `steam.exe` 同级）。不创建则使用内置默认（四工具默认开）。文件被监视，改动热生效：

```toml
[tools.enabled]
catalog_add = true        # 入库 / 清单
library_ux = true         # 库 UX
config_ui = true          # 配置页
download_kit = true       # 下载套件（默认开）
store_accel = false       # 商店加速（默认关）

[catalog]
mode = "disabled"         # disabled / custom_http / lua / community / mock
url_template = ""         # custom_http 时: https://example.com/catalog/{app_id}

[manifest]
url = "opensteamtool"     # manifest request code 源: opensteamtool / steamrun / wudrm

[store_accel]
egress = "disabled"       # disabled / direct_dns / local_cdn / http_connect
clash_fallback = "127.0.0.1:7890"   # 可选：仅允许 loopback，本地候选失败后最多回退一次
```

## Steam 版本兼容

- 不内置硬编码偏移：每次启动按 `steamclient64.dll` / `steamui.dll` 的 **SHA-256** 匹配外置 pattern
- 优先从 `SteamTools-Patterns` 拉取（按本机 DLL SHA 原子缓存到 `steamtools/pattern/{component}/{sha}.toml`）；远端不可用或不匹配时回退内置兼容 pattern 与 `opensteamtool/pattern` 路径
- pattern 缺失只禁用对应能力（UI 贡献 / hook），其余工具照常运行；host.log 与工具中心会写明降级原因

## 构建

产物（MSVC release）：

| 产物 | 输出路径 |
|------|----------|
| `SteamTools.dll` | `target\release\steamtools.dll`（`stt-host` cdylib 输出名） |
| `dwmapi.dll` | `target\release\dwmapi.dll`（`stt-loader-dwmapi`） |
| `xinput1_4.dll` | `target\release\xinput1_4.dll`（`stt-loader-xinput`） |

配置面板 bundle 单独构建（已提交产物，普通 `cargo build` 不调用 Node）：

```powershell
cd crates\stt-steamui\ui
npm ci && npm run check
```

## 架构

```text
dwmapi.dll / xinput1_4.dll        # 纯 Rust 加载器 → LoadLibrary(SteamTools.dll)
        └→ SteamTools.dll (stt-host)
             ├─ stt-config         TOML + Lua 配置、热重载、原子落盘
             ├─ stt-core           入库状态 / AppRules 唯一状态源
             ├─ stt-catalog        CatalogProvider 链（多源聚合、集中校验）
             ├─ stt-steamui        CDP 注入：入库按钮 / 配置面板 / 库右键菜单
             ├─ stt-steamclient    拥有权 / 清单 / key / token / request code
             └─ stt-store-accel    商店加速（DLL 内线程，PAC + CONNECT）
```

配套：`stt-metadata`（pattern 解析）· `stt-hook`（可卸载 detour）· `stt-platform`（受限 WinHTTP / 文件系统）。

## 边界与声明

- **「入库」= 元数据落盘**，不宣称下载完成；下载相关能力是独立的 `download_kit`，默认开启、可在配置页关闭
- `store_accel` 只处理商店 / 社区 / 创意工坊网页流量，不代理下载、游戏、P2P 与票据
- 能力随 Steam 版本漂移：pattern 失效时对应能力单独降级，其余工具不受影响

## 免责声明

1. **研究用途** — 本项目仅用于工程 / 研究 / 学习，请勿用于商业、违规或任何未经授权的用途
2. **封号风险** — 入库、下载套件等能力可能违反 Steam 服务条款，存在账号风险；**强烈建议使用小号测试**，由此造成的封号、财产损失由使用者自行承担
3. **非官方产品** — 本项目与 Valve / Steam 无任何关联，非官方出品，不提供任何形式的官方支持
4. **数据与隐私** — 本项目不上传账号、密钥、ticket 等敏感数据；日志仅写本机 `steamtools/host.log` 且已脱敏
5. **后果自负** — 使用本项目产生的任何后果由使用者自行承担

> [!CAUTION]
> 使用本项目即视为接受以上条款。

## 致谢

- [OpenSteamTool](https://github.com/OpenSteam001/OpenSteamTool) — 能力与 Lua 生态参考，本项目的产品形态与实现均为独立设计
- [Steamcommunity 302](https://www.dogfight360.com/blog/18682/) - 商店及社区解锁思路
- [Feather Icons](https://feathericons.com/) (MIT) - logo 入库图标基础

## 许可证

本项目采用 [GPLv3](./LICENSE) 协议开源；漏洞报告见 [SECURITY.md](SECURITY.md)。

