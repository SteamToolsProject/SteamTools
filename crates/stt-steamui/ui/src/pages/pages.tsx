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
  return <><Lead title="数据源" description="选择目录、清单和日志使用的来源" /><div className="stt-group-label">数据源</div><FormRow title="Catalog 源" detail={snapshot.catalog_status} control={<Segmented label="Catalog 源" values={snapshot.catalog_modes} current={snapshot.catalog_mode} labels={CATALOG_LABELS} onChange={(value) => emitIntent({ kind: "set_catalog_mode", value })} />} /><FormRow title="自动添加 DLC" detail="解锁全部 DLC 所有权; 有清单+密钥的会一并完整入库; 失败不影响主游戏" control={<Toggle checked={snapshot.catalog_auto_dlc} label="切换自动添加 DLC" onChange={() => emitIntent({ kind: "set_catalog_auto_dlc", on: !snapshot.catalog_auto_dlc })} />} /><div className="stt-row"><div className="stt-row-main"><div className="stt-row-title">Catalog URL 模板</div><div className="stt-row-detail">填一个带 {"{app_id}"} 占位符的 http(s) 地址, 保存时校验</div>{snapshot.catalog_mode !== "custom_http" ? <div className="stt-row-detail">仅在 CustomHttp 模式下生效</div> : null}{catalogError ? <div className="stt-field-error" id="catalog-url-error">{catalogError}</div> : null}</div><div className="stt-actions"><input className={`stt-input${catalogError ? " stt-input-error" : ""}`} aria-invalid={catalogError ? "true" : undefined} aria-describedby={catalogError ? "catalog-url-error" : undefined} placeholder="https://catalog.example/v1/{app_id}" value={catalogDraftValue} onInput={(event) => { setCatalogBusy(false); setCatalogPending(null); setCatalogError(""); setCatalogDraft((event.currentTarget as HTMLInputElement).value); }} onKeyDown={(event) => { if (event.key === "Enter") saveCatalog(); }} /><Button disabled={catalogBusy} onClick={saveCatalog}>{catalogBusy ? "保存中" : "保存 URL"}</Button></div></div><div className="stt-group-label">清单请求码</div><FormRow title="Manifest 请求码源" detail="下载时向所选服务请求清单码" control={<Segmented label="Manifest 请求码源" values={snapshot.manifest_sources} current={snapshot.manifest_url} onChange={(value) => emitIntent({ kind: "set_manifest_url", value })} />} /><div className="stt-group-label">日志</div><FormRow title="日志级别" detail="控制日志记录的详细程度" control={<Segmented label="日志级别" values={snapshot.log_levels} current={snapshot.log_level} onChange={(value) => emitIntent({ kind: "set_log_level", value })} />} /></>;
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

export function LuaPage({ snapshot }: { snapshot: ConfigSnapshot }) {
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [pendingPath, setPendingPath] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [pendingRemove, setPendingRemove] = useState<string | null>(null);
  useEffect(() => {
    if (snapshot.note.startsWith("失败")) {
      if (pendingPath) setDraft(pendingPath);
      setBusy(false);
      setPendingPath(null);
      setPendingRemove(null);
      setConfirming(null);
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
  }, [pendingPath, pendingRemove, snapshot.lua_paths, snapshot.note]);
  const submit = () => {
    const value = draft.trim();
    if (!value) return;
    setBusy(true);
    setPendingPath(value);
    setDraft("");
    emitIntent({ kind: "add_lua_path", value });
  };
  return <><Lead title="脚本目录" description="按顺序加载配置目录中的脚本" /><FormRow title={snapshot.lua_dir} detail="总是加载, 不可移除" mono control={<span className="stt-spacer-value">内置</span>} />{snapshot.lua_paths.map((path) => { const isPending = pendingRemove === path; const isConfirming = confirming === path; return <FormRow key={path} title={path} mono control={isConfirming ? <div className="stt-actions"><Button onClick={() => setConfirming(null)}>取消</Button><Button danger disabled={isPending} onClick={() => { setPendingRemove(path); emitIntent({ kind: "remove_lua_path", value: path }); }}>{isPending ? "移除中" : "确认移除"}</Button></div> : <Button danger disabled={Boolean(pendingRemove)} onClick={() => setConfirming(path)}>移除</Button>} />; })}<div className="stt-row"><div className="stt-row-main"><div className="stt-row-title">添加脚本目录</div><div className="stt-row-detail">需要是存在的绝对路径</div></div><div className="stt-actions"><input className="stt-input" placeholder="输入脚本目录的绝对路径" value={draft} onInput={(event) => { setBusy(false); setPendingPath(null); setDraft((event.currentTarget as HTMLInputElement).value); }} onKeyDown={(event) => { if (event.key === "Enter") submit(); }} /><Button disabled={busy} onClick={submit}>{busy ? "添加中" : "添加"}</Button></div></div></>;
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
  return snapshot.note ? <Notice message={snapshot.note} /> : null;
}
