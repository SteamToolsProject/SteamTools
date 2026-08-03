import { render } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import css from "./panel.css";
import { ConfigSnapshot, EMPTY_SNAPSHOT } from "./bridge";
import { SideNav, Page, pageTitle } from "./components/nav";
import { PageBody, PageNotice } from "./pages/pages";
import { LoadingState } from "./components/ui";

function PanelApp({ snapshot, onClose }: { snapshot: ConfigSnapshot; onClose: () => void }) {
  const [page, setPage] = useState<Page>("tools");
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    closeRef.current?.focus();
    return () => { if (previous?.isConnected) previous.focus(); };
  }, []);
  return (
    <div className="stt-dialog" role="dialog" aria-modal="true" aria-label="SteamTools 设置">
      <aside className="stt-sidebar">
        <div className="stt-brand">
          <div className="stt-brand-mark" aria-hidden="true">ST</div>
          <div>
            <div className="stt-brand-name">STEAMTOOLS</div>
            <div className="stt-brand-sub">控制中心</div>
          </div>
        </div>
        <div className="stt-sidebar-label">配置</div>
        <SideNav page={page} onSelect={setPage} />
        <div className="stt-sidebar-footer">版本 v{snapshot.version || "?"} · 本地运行</div>
      </aside>
      <section className="stt-pane">
        <header className="stt-header">
          <div>
            <div className="stt-eyebrow">SteamTools / 配置</div>
            <div className="stt-title">{pageTitle(page)}<span className={`stt-connection${snapshot.channel ? " stt-connection-on" : ""}`}><span className="stt-connection-dot" />{snapshot.channel ? "已连接" : "等待连接"}</span></div>
          </div>
          <button ref={closeRef} type="button" className="stt-close" aria-label="关闭设置" onClick={onClose}><span aria-hidden="true">×</span></button>
        </header>
        <main className="stt-body">{snapshot.tools.length ? <PageBody page={page} snapshot={snapshot} /> : <LoadingState />}</main>
        <PageNotice snapshot={snapshot} />
      </section>
    </div>
  );
}

(function mount() {
  const documentRoot = document;
  if (documentRoot.getElementById("stt-panel")) return "already";

  const root = documentRoot.createElement("div");
  root.id = "stt-panel";
  const style = documentRoot.createElement("style");
  style.textContent = css;
  root.appendChild(style);
  (documentRoot.body || documentRoot.documentElement).appendChild(root);

  let setSnapshot: ((snapshot: ConfigSnapshot) => void) | undefined;
  let disposed = false;
  const navRow = window.__SteamToolsNavRow ?? null;
  const onKey = (event: KeyboardEvent) => {
    if (event.key === "Escape") close();
    if (event.key !== "Tab" || !root.contains(document.activeElement)) return;
    const focusable = Array.from(root.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])'));
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };
  const onRootClick = (event: MouseEvent) => {
    if (event.target === root) close();
  };
  const onNavClick = (event: Event) => {
    let target = event.target as Element | null;
    while (target && target !== navRow) {
      if (target.getAttribute("data-stt-nav") !== null) return;
      target = target.parentElement;
    }
    close();
  };
  const cleanup = () => {
    documentRoot.removeEventListener("keydown", onKey, true);
    root.removeEventListener("click", onRootClick);
    navRow?.removeEventListener("click", onNavClick, true);
  };
  function close() {
    if (disposed) return;
    disposed = true;
    window.__SteamToolsWant = false;
    window.__SteamToolsClosed = true;
    window.__SteamToolsClose = null;
    cleanup();
    render(null, root);
    root.remove();
  }

  documentRoot.addEventListener("keydown", onKey, true);
  root.addEventListener("click", onRootClick);
  if (navRow) navRow.addEventListener("click", onNavClick, true);
  window.__SteamToolsClose = close;
  window.__SteamToolsIntents = window.__SteamToolsIntents ?? [];
  window.__SteamToolsPanel = { update(snapshot) { setSnapshot?.(snapshot); } };

  function App() {
    const [snapshot, update] = useState<ConfigSnapshot>(EMPTY_SNAPSHOT);
    setSnapshot = update;
    return <PanelApp snapshot={snapshot} onClose={close} />;
  }
  render(<App />, root);
  return "opened";
})();
