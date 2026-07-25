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

- [x] workspace：`ost-core` / `ost-platform` / `ost-host` / loaders
- [x] M1 代码：双 loader + `SteamTools.dll` + `steamtools/host.log`（待本机 Steam 冒烟）

```powershell
cargo test
cargo build -p ost-host -p ost-loader-dwmapi -p ost-loader-xinput --release
```

产物：`SteamTools.dll`、`dwmapi.dll`、`xinput1_4.dll` → 复制到 Steam 根（见本机 `docs/plan/smoke-m1.md`）。

## 架构（目标）

```text
loader (dwmapi/xinput) → ost-host
  → ost-config (TOML + mlua) / ost-metadata / ost-platform
  → ost-core ← ost-hook ← ost-steamui + ost-steamclient
```

- **steamui**：库体验与配置 UX  
- **steamclient**：拥有权 / 下载 / 密钥等（不可省）  
