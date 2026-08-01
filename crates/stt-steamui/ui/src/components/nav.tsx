import { h } from "preact";
import { Icon } from "./ui";

export type Page = "tools" | "source" | "apps" | "lua" | "status";

const PAGES: Array<{ id: Page; label: string; path: string }> = [
  { id: "tools", label: "工具", path: "M4 8h3m4 0h9M4 16h9m4 0h3M11 8a2 2 0 10-4 0 2 2 0 004 0M17 16a2 2 0 10-4 0 2 2 0 004 0" },
  { id: "source", label: "数据源", path: "M12 3v12m0 0l-4-4m4 4l4-4M4 19h16" },
  { id: "apps", label: "已入库", path: "M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4" },
  { id: "lua", label: "脚本目录", path: "M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2z" },
  { id: "status", label: "状态", path: "M12 21a9 9 0 100-18 9 9 0 000 18zM12 8h.01M11 12h1v5h1" }
];

export function SideNav({ page, onSelect }: { page: Page; onSelect: (page: Page) => void }) {
  return <nav aria-label="配置分区">{PAGES.map((item) => <button type="button" key={item.id} className={`stt-nav${item.id === page ? " stt-nav-active" : ""}`} aria-current={item.id === page ? "page" : undefined} onClick={() => onSelect(item.id)}><Icon path={item.path} /><span>{item.label}</span></button>)}</nav>;
}

export function pageTitle(page: Page): string {
  return PAGES.find((item) => item.id === page)?.label ?? "设置";
}
