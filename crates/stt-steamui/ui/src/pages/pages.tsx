import { h } from "preact";
import { useEffect, useMemo, useState } from "preact/hooks";
import { ConfigSnapshot, emitIntent } from "../bridge";
import { Button, EmptyState, FormRow, Lead, Notice, Segmented, Toggle } from "../components/ui";

const CATALOG_LABELS: Record<string, string> = { disabled: "关闭", custom_http: "CustomHttp", lua: "Lua", community: "Community", mock: "Mock" };

const TOOL_DESCRIPTIONS: Record<string, string> = {
  catalog_add: "在商店页面添加入库按钮",
  library_ux: "增强库内操作",
  config_ui: "打开 SteamTools 配置面板",
  download_kit: "为 Steam 下载提供清单、密钥与令牌数据, 不加速也不代下载",
  store_accel: "优化商店访问",
  lua_drop: "在「脚本目录」页拖入 .lua 文件即可导入",
};

function appNameFromSteamDom(appId: number): string {
  if (typeof document === "undefined") return `Steam 应用 ${appId}`;
  const id = String(appId);
  const selectors = [
    `[data-appid="${id}"]`,
    `[data-app-id="${id}"]`,
    `img[src*="/apps/${id}/"]`,
    `a[href*="/app/${id}/"]`,
  ];
  const clean = (value: string | null | undefined): string | null => {
    const text = (value || "").replace(/\s+/g, " ").trim();
    if (!text || text === id || /^\d+$/.test(text) || text.length > 90) return null;
    return text;
  };
  for (const selector of selectors) {
    for (const node of Array.from(document.querySelectorAll<HTMLElement>(selector))) {
      for (const labelled of [node.getAttribute("aria-label"), node.getAttribute("data-tooltip-text"), node.getAttribute("title"), node.getAttribute("alt")]) {
        const value = clean(labelled);
        if (value) return value;
      }
      let current: HTMLElement | null = node;
      for (let depth = 0; current && depth < 7; depth += 1, current = current.parentElement) {
        const value = clean(current.innerText || current.textContent);
        if (value && !value.includes("刷新") && !value.includes("移除")) return value;
      }
    }
  }
  return `Steam 应用 ${appId}`;
}

function toolDetail(tool: ConfigSnapshot["tools"][number]): string {
  if (tool.placeholder) return "开发中";
  if (!tool.enabled) return "当前关闭";
  return TOOL_DESCRIPTIONS[tool.id] || "功能已启用";
}

export function ToolsPage({ snapshot }: { snapshot: ConfigSnapshot }) {
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const enabled = snapshot.tools.filter((tool) => tool.enabled).length;
  useEffect(() => {
    setPending((old) => {
      const next = { ...old };
      let changed = false;
      for (const tool of snapshot.tools) {
        if (next[tool.id] === tool.enabled) {
          delete next[tool.id];
          changed = true;
        }
      }
      if (snapshot.note.startsWith("失败")) return Object.keys(next).length ? {} : old;
      return changed ? next : old;
    });
  }, [snapshot.tools, snapshot.note]);
  return <><Lead title="工具" description={`${enabled} / ${snapshot.tools.length} 项启用`} />{snapshot.tools.map((tool) => { const desired = pending[tool.id]; const isPending = desired !== undefined; return <FormRow key={tool.id} title={tool.name} detail={isPending ? "正在保存..." : toolDetail(tool)} control={<Toggle checked={isPending ? desired : tool.enabled} disabled={tool.placeholder || isPending} label={`${tool.placeholder ? "查看" : "切换"} ${tool.name}`} onChange={() => { const on = !tool.enabled; setPending((old) => ({ ...old, [tool.id]: on })); emitIntent({ kind: "set_tool", id: tool.id, on }); }} />} />; })}</>;
}

export function SourcesPage({ snapshot }: { snapshot: ConfigSnapshot }) {
  const [catalogDraft, setCatalogDraft] = useState<string | null>(null);
  const catalogDraftValue = catalogDraft ?? snapshot.catalog_url_template;
  const [catalogError, setCatalogError] = useState("");
  const [catalogBusy, setCatalogBusy] = useState(false);
  const [catalogPending, setCatalogPending] = useState<string | null>(null);
  useEffect(() => {
    if (!catalogPending) return;
    if (snapshot.note.startsWith("失败")) {
      setCatalogBusy(false);
      setCatalogPending(null);
    } else if (snapshot.catalog_url_template === catalogPending) {
      setCatalogBusy(false);
      setCatalogPending(null);
      setCatalogDraft(null);
    }
  }, [catalogPending, snapshot.catalog_url_template, snapshot.note]);
  const saveCatalog = () => {
    const value = catalogDraftValue.trim();
    if (!/^https?:\/\//.test(value) || value.split("{app_id}").length !== 2) {
      setCatalogError("请输入 http(s) 地址, 并保留 {app_id} 占位符");
      return;
    }
    setCatalogError("");
    setCatalogBusy(true);
    setCatalogPending(value);
    emitIntent({ kind: "set_catalog_url_template", value });
  };
  return <><Lead title="数据源" description="选择目录、清单和日志使用的来源" /><div className="stt-group-label">数据源</div><FormRow title="Catalog 源" detail={snapshot.catalog_status} control={<Segmented label="Catalog 源" values={snapshot.catalog_modes} current={snapshot.catalog_mode} labels={CATALOG_LABELS} onChange={(value) => emitIntent({ kind: "set_catalog_mode", value })} />} /><FormRow title="自动添加 DLC" detail="默认影响刷新清单与 Inbox: 开=全量 DLC, 关=仅游戏. 商店主击仍是全入库, 可用 ▾ 选仅游戏或勾选 DLC" control={<Toggle checked={snapshot.catalog_auto_dlc} label="切换自动添加 DLC" onChange={() => emitIntent({ kind: "set_catalog_auto_dlc", on: !snapshot.catalog_auto_dlc })} />} /><div className="stt-row"><div className="stt-row-main"><div className="stt-row-title">Catalog URL 模板</div><div className="stt-row-detail">填一个带 {"{app_id}"} 占位符的 http(s) 地址, 保存时校验</div>{snapshot.catalog_mode !== "custom_http" ? <div className="stt-row-detail">仅在 CustomHttp 模式下生效</div> : null}{catalogError ? <div className="stt-field-error" id="catalog-url-error">{catalogError}</div> : null}</div><div className="stt-actions"><input className={`stt-input${catalogError ? " stt-input-error" : ""}`} aria-invalid={catalogError ? "true" : undefined} aria-describedby={catalogError ? "catalog-url-error" : undefined} placeholder="https://catalog.example/v1/{app_id}" value={catalogDraftValue} onInput={(event) => { setCatalogBusy(false); setCatalogPending(null); setCatalogError(""); setCatalogDraft((event.currentTarget as HTMLInputElement).value); }} onKeyDown={(event) => { if (event.key === "Enter") saveCatalog(); }} /><Button disabled={catalogBusy} onClick={saveCatalog}>{catalogBusy ? "保存中" : "保存 URL"}</Button></div></div><div className="stt-group-label">清单请求码</div><FormRow title="Manifest 请求码源" detail="下载时向所选服务请求清单码" control={<Segmented label="Manifest 请求码源" values={snapshot.manifest_sources} current={snapshot.manifest_url} onChange={(value) => emitIntent({ kind: "set_manifest_url", value })} />} /><div className="stt-group-label">日志</div><FormRow title="日志级别" detail="控制日志记录的详细程度" control={<Segmented label="日志级别" values={snapshot.log_levels} current={snapshot.log_level} onChange={(value) => emitIntent({ kind: "set_log_level", value })} />} /></>;
}

export function AppsPage({ snapshot }: { snapshot: ConfigSnapshot }) {
  const [busy, setBusy] = useState<Record<string, string>>({});
  const [confirming, setConfirming] = useState<number | null>(null);
  const managedKey = snapshot.managed.join(",");
  const nameKey = snapshot.managed.map((id) => `${id}:${snapshot.managed_names[String(id)] || ""}`).join(",");
  const names = useMemo(() => new Map(snapshot.managed.map((id) => [id, snapshot.managed_names[String(id)] || appNameFromSteamDom(id)])), [managedKey, nameKey, snapshot.note]);
  useEffect(() => {
    if (snapshot.note.startsWith("正在")) return;
    setBusy({});
    setConfirming(null);
  }, [managedKey, snapshot.note]);
  if (!snapshot.managed.length) return <><Lead title="管理列表" description="从商店详情页入库的应用会出现在这里" /><EmptyState title="还没有入库的应用" detail="打开 Steam 商店详情页, 使用入库按钮添加应用" /></>;
  return <><Lead title="管理列表" description={`${snapshot.managed.length} 个应用由 SteamTools 管理`} />{snapshot.managed.map((id) => <FormRow key={id} title={names.get(id) || `Steam 应用 ${id}`} detail={`AppID ${id} · 已入库`} control={<div className="stt-actions">{confirming === id ? <><Button onClick={() => setConfirming(null)}>取消</Button><Button danger disabled={busy[id] === "remove"} onClick={() => { setBusy((old) => ({ ...old, [id]: "remove" })); emitIntent({ kind: "remove_app", app_id: id }); }}>{busy[id] === "remove" ? "移除中" : "确认移除"}</Button></> : <><Button disabled={busy[id] === "refresh"} onClick={() => { setBusy((old) => ({ ...old, [id]: "refresh" })); emitIntent({ kind: "refresh_app", app_id: id }); }}>{busy[id] === "refresh" ? "刷新中" : "刷新清单"}</Button><Button onClick={() => setConfirming(id)}>移除</Button></>}</div>} />)}</>;
}

async function readLuaFiles(fileList: FileList | File[]): Promise<Array<{ name: string; body: string }>> {
  const files = Array.from(fileList as ArrayLike<File>);
  const out: Array<{ name: string; body: string }> = [];
  for (const file of files) {
    const name = String(file.name || "");
    if (!/\.lua$/i.test(name)) continue;
    if (file.size > 2 * 1024 * 1024) continue;
    // CEF 上 File.text() 偶发不可用, 回退 FileReader.
    let body = "";
    try {
      if (typeof file.text === "function") body = await file.text();
      else {
        body = await new Promise<string>((resolve, reject) => {
          const reader = new FileReader();
          reader.onload = () => resolve(String(reader.result || ""));
          reader.onerror = () => reject(reader.error || new Error("read failed"));
          reader.readAsText(file);
        });
      }
    } catch {
      continue;
    }
    if (!body.trim()) continue;
    out.push({ name, body });
    if (out.length >= 16) break;
  }
  return out;
}

/** CEF / 某些拖源只给 items 不给 files; 文件夹则走 webkit entry. */
async function filesFromDataTransfer(dt: DataTransfer | null | undefined): Promise<File[]> {
  if (!dt) return [];
  const out: File[] = [];
  const pushFile = (file: File | null | undefined) => {
    if (!file) return;
    if (out.some((f) => f.name === file.name && f.size === file.size)) return;
    out.push(file);
  };

  if (dt.items && dt.items.length) {
    const entries: Array<any> = [];
    for (let i = 0; i < dt.items.length; i += 1) {
      const item = dt.items[i];
      if (!item) continue;
      const entry = typeof item.webkitGetAsEntry === "function" ? item.webkitGetAsEntry() : null;
      if (entry) entries.push(entry);
      else if (item.kind === "file") pushFile(item.getAsFile());
    }
    if (entries.length) {
      const stack = entries.slice();
      while (stack.length && out.length < 64) {
        const entry = stack.pop();
        if (!entry) continue;
        if (entry.isFile) {
          const file: File | null = await new Promise((resolve) => {
            try {
              entry.file((f: File) => resolve(f), () => resolve(null));
            } catch {
              resolve(null);
            }
          });
          pushFile(file);
        } else if (entry.isDirectory && typeof entry.createReader === "function") {
          const reader = entry.createReader();
          const kids: any[] = await new Promise((resolve) => {
            const acc: any[] = [];
            const pump = () => {
              try {
                reader.readEntries(
                  (batch: any[]) => {
                    if (!batch || !batch.length) resolve(acc);
                    else {
                      acc.push(...batch);
                      pump();
                    }
                  },
                  () => resolve(acc),
                );
              } catch {
                resolve(acc);
              }
            };
            pump();
          });
          for (const kid of kids) stack.push(kid);
        }
      }
    }
  }

  if (!out.length && dt.files && dt.files.length) {
    for (let i = 0; i < dt.files.length; i += 1) pushFile(dt.files.item(i));
  }
  return out;
}

function allowDrop(event: any, enabled: boolean): void {
  // 必须 preventDefault + dropEffect=copy, 否则 CEF/Chromium 显示禁止光标且不触发 drop.
  try { event.preventDefault(); } catch { /* ignore */ }
  try { event.stopPropagation(); } catch { /* ignore */ }
  try {
    const dt = event.dataTransfer;
    if (dt) {
      dt.dropEffect = enabled ? "copy" : "none";
    }
  } catch {
    // ignore
  }
}

export function LuaPage({ snapshot }: { snapshot: ConfigSnapshot }) {
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [pendingPath, setPendingPath] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [pendingRemove, setPendingRemove] = useState<string | null>(null);
  const [importBusy, setImportBusy] = useState(false);
  const [dragOver, setDragOver] = useState(false);
  const [localNote, setLocalNote] = useState("");
  const dropEnabled = snapshot.tools.some((tool) => tool.id === "lua_drop" && tool.enabled);
  // CEF 上 OS 文件拖入常被 document 默认行为拦掉; 面板打开时在捕获阶段放行.
  useEffect(() => {
    if (!dropEnabled || typeof document === "undefined") return;
    const onDragOver = (event: Event) => {
      const e = event as any;
      try {
        const t = e.target as Element | null;
        if (!t || typeof t.closest !== "function" || !t.closest("#stt-panel")) return;
        e.preventDefault();
        if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
      } catch {
        // ignore
      }
    };
    const onDrop = (event: Event) => {
      const e = event as any;
      try {
        const t = e.target as Element | null;
        if (!t || typeof t.closest !== "function" || !t.closest(".stt-dropzone")) return;
        e.preventDefault();
      } catch {
        // ignore
      }
    };
    document.addEventListener("dragover", onDragOver, true);
    document.addEventListener("drop", onDrop, true);
    return () => {
      document.removeEventListener("dragover", onDragOver, true);
      document.removeEventListener("drop", onDrop, true);
    };
  }, [dropEnabled]);
  useEffect(() => {
    if (snapshot.note.startsWith("失败")) {
      if (pendingPath) setDraft(pendingPath);
      setBusy(false);
      setPendingPath(null);
      setPendingRemove(null);
      setConfirming(null);
      setImportBusy(false);
      return;
    }
    if (pendingPath && snapshot.lua_paths.includes(pendingPath)) {
      setBusy(false);
      setPendingPath(null);
    }
    if (pendingRemove && !snapshot.lua_paths.includes(pendingRemove)) {
      setPendingRemove(null);
      setConfirming(null);
    }
    if (importBusy && (snapshot.note.startsWith("已导入") || snapshot.note.startsWith("失败") || snapshot.note.startsWith("正在导入"))) {
      if (!snapshot.note.startsWith("正在导入")) setImportBusy(false);
      setLocalNote("");
    }
  }, [pendingPath, pendingRemove, importBusy, snapshot.lua_paths, snapshot.note]);
  const submit = () => {
    const value = draft.trim();
    if (!value) return;
    setBusy(true);
    setPendingPath(value);
    setDraft("");
    emitIntent({ kind: "add_lua_path", value });
  };
  const importFiles = async (list: FileList | File[] | null, source: "drop" | "picker") => {
    if (!dropEnabled) {
      setLocalNote("拖放导入已关闭, 请到「工具」页打开");
      return;
    }
    if (importBusy) return;
    setImportBusy(true);
    setLocalNote(source === "drop" ? "正在读取拖入的文件…" : "正在读取所选文件…");
    try {
      const files = await readLuaFiles(list || []);
      if (!files.length) {
        setImportBusy(false);
        setLocalNote(
          source === "drop"
            ? "没有识别到 .lua（可拖文件或文件夹；CEF 若仍禁止拖入请点「选择文件」）"
            : "没有识别到 .lua 文件",
        );
        return;
      }
      setLocalNote(`正在导入 ${files.length} 个 lua…`);
      emitIntent({ kind: "import_lua", files });
    } catch {
      setImportBusy(false);
      setLocalNote("读取文件失败, 请重试或改用「选择文件」");
    }
  };
  return <>
    <Lead title="脚本目录" description="按顺序加载配置目录中的脚本" />
    <div
      className={`stt-dropzone${dragOver ? " stt-dropzone-active" : ""}${!dropEnabled ? " stt-dropzone-off" : ""}`}
      onDragEnter={(event) => { allowDrop(event, dropEnabled); if (dropEnabled) setDragOver(true); }}
      onDragOver={(event) => { allowDrop(event, dropEnabled); if (dropEnabled) setDragOver(true); }}
      onDragLeave={(event) => {
        // 只在真正离开容器时取消高亮, 避免子节点抖动.
        const next = (event as any).relatedTarget as Node | null;
        if (next && (event.currentTarget as HTMLElement).contains(next)) return;
        setDragOver(false);
      }}
      onDrop={(event) => {
        allowDrop(event, dropEnabled);
        setDragOver(false);
        if (!dropEnabled) {
          setLocalNote("拖放导入已关闭, 请到「工具」页打开");
          return;
        }
        // DataTransfer 在 drop 回调返回后会失效; 必须同步摘 File,
        // 目录 entry 的异步读取尽量在同一 tick 启动.
        const dt = event.dataTransfer;
        const syncFiles: File[] = [];
        if (dt?.files) {
          for (let i = 0; i < dt.files.length; i += 1) {
            const f = dt.files.item(i);
            if (f) syncFiles.push(f);
          }
        }
        void (async () => {
          let files = syncFiles;
          if (!files.length) {
            try {
              files = await filesFromDataTransfer(dt);
            } catch {
              files = [];
            }
          }
          await importFiles(files, "drop");
        })();
      }}
    >
      <div className="stt-dropzone-title">{importBusy ? "正在导入…" : dropEnabled ? "拖入 .lua 文件 / 文件夹" : "拖放导入已关闭"}</div>
      <div className="stt-dropzone-detail">{dropEnabled ? "可一次多个; 写入 config/lua 并热重载。若拖入被系统禁止, 请点「选择文件」" : "在「工具」页打开「拖放导入」后可用"}</div>
      {localNote ? <div className="stt-dropzone-local">{localNote}</div> : null}
      {dropEnabled ? <label className="stt-dropzone-pick"><input type="file" accept=".lua,text/plain,text/*" multiple disabled={importBusy} onChange={(event) => { void importFiles(event.currentTarget.files, "picker"); event.currentTarget.value = ""; }} />{importBusy ? "导入中" : "选择文件"}</label> : null}
    </div>
    <FormRow title={snapshot.lua_dir} detail="总是加载, 不可移除" mono control={<span className="stt-spacer-value">内置</span>} />
    {snapshot.lua_paths.map((path) => {
      const isPending = pendingRemove === path;
      const isConfirming = confirming === path;
      return <FormRow key={path} title={path} mono control={isConfirming ? <div className="stt-actions"><Button onClick={() => setConfirming(null)}>取消</Button><Button danger disabled={isPending} onClick={() => { setPendingRemove(path); emitIntent({ kind: "remove_lua_path", value: path }); }}>{isPending ? "移除中" : "确认移除"}</Button></div> : <Button danger disabled={Boolean(pendingRemove)} onClick={() => setConfirming(path)}>移除</Button>} />;
    })}
    <div className="stt-row"><div className="stt-row-main"><div className="stt-row-title">添加脚本目录</div><div className="stt-row-detail">需要是存在的绝对路径</div></div><div className="stt-actions"><input className="stt-input" placeholder="输入脚本目录的绝对路径" value={draft} onInput={(event) => { setBusy(false); setPendingPath(null); setDraft((event.currentTarget as HTMLInputElement).value); }} onKeyDown={(event) => { if (event.key === "Enter") submit(); }} /><Button disabled={busy} onClick={submit}>{busy ? "添加中" : "添加"}</Button></div></div>
  </>;
}

export function StatusPage({ snapshot }: { snapshot: ConfigSnapshot }) {
  const enabled = snapshot.tools.filter((tool) => tool.enabled).length;
  const rows: Array<[string, string, string]> = [["连接方式", snapshot.channel || "等待连接", "SteamTools 与客户端界面之间的连接"], ["已加载应用", String(snapshot.owned_count), "当前配置中可识别的应用数量"], ["已启用工具", `${enabled} / ${snapshot.tools.length}`, "当前打开的功能数量"], ["版本", `v${snapshot.version || "?"}`, ""], ["自动更新", snapshot.update_status || "检查中", "新版本就绪后重启 Steam 生效"]];
  return <><Lead title="运行状态" description="当前会话的运行信息" />{rows.map(([title, value, detail]) => <FormRow key={title} title={title} detail={detail} control={<span className="stt-value-mono stt-spacer-value">{value}</span>} />)}</>;
}

export function PageBody({ page, snapshot }: { page: string; snapshot: ConfigSnapshot }) {
  if (page === "source") return <SourcesPage snapshot={snapshot} />;
  if (page === "apps") return <AppsPage snapshot={snapshot} />;
  if (page === "lua") return <LuaPage snapshot={snapshot} />;
  if (page === "status") return <StatusPage snapshot={snapshot} />;
  return <ToolsPage snapshot={snapshot} />;
}

export function PageNotice({ snapshot }: { snapshot: ConfigSnapshot }) {
  // 空 note 不占底部空间; 有内容时贴在 pane 底部.
  return snapshot.note ? <Notice message={snapshot.note} /> : null;
}
