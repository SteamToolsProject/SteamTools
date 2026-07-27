# SteamTools

**SteamTools** — Windows 下 Steam 进程内工具箱（Rust）：配置页 / 插件 / 库增强；商店「入库」拉清单。  
参考 [OpenSteamTool](https://github.com/OpenSteam001/OpenSteamTool)

> 研究 / 工程练习向。请遵守当地法律与 Steam 服务条款。

配置：`steamtools.toml` · 数据目录：`steamtools/` · 产物：`SteamTools.dll` + loader DLL。

## 仓库里有什么（Git 默认只跟踪代码）

| 路径 | 说明 |
|------|------|
| `crates/` | Rust workspace 成员 |
| `Cargo.toml` | workspace 清单 |
| `rust-toolchain.toml` | 工具链 |
| `README.md` | 本说明（可选提交） |

## 当前状态

- [x] workspace：`stt-core` / `stt-catalog` / `stt-platform` / `stt-config` / `stt-metadata` / `stt-hook` / `stt-steamui` / `stt-steamclient` / `stt-host` / loaders
- [x] M1：双 loader + `SteamTools.dll` + `host.log`（本机已冒烟）
- [x] M2：`steamtools.toml`、工具开关、mlua、Catalog Mock、lua 扫盘/watch
- [x] M3：SHA-256、pattern TOML、符号解析、可卸载 detour、无害自测 hook
- [x] M4a：库 UX 状态机 + 可降级安装规划 (尚无业务 detour)
- [x] M4b：商店「入库」按钮 (CDP 注入 + 页内队列回传) → `config/lua/stt_{app_id}.lua`
- [x] M4c：配置页 (标签行入口 + 面板 → `steamtools.toml` 热生效，本机已冒烟)
- [x] M6-1：`stt-catalog`、Catalog wire v1、集中校验与 AppId/DepotId 明确建模
- [x] M6-2：受限 WinHTTP、`CustomHttp` 真 Catalog、独立 `[catalog]` 配置与后台 worker

```powershell
cargo test
cargo build -p stt-host -p stt-loader-dwmapi -p stt-loader-xinput --release
```

产物：`SteamTools.dll`、`dwmapi.dll`、`xinput1_4.dll` → 复制到 Steam 根（见本机 `docs/plan/smoke-m1.md`）。

## 架构（目标）

```text
loader (dwmapi/xinput) → stt-host
  → stt-catalog (provider + wire) / stt-config (TOML + mlua + 持久化)
  → stt-metadata / stt-platform
  → stt-core ← stt-hook（M3 脚手架）← stt-steamui + stt-steamclient（后置）
```

- **steamui**：库体验与配置 UX  
- **steamclient**：拥有权 / 下载 / 密钥等（不可省）  
