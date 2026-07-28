export interface ToolView {
  id: string;
  name: string;
  enabled: boolean;
  placeholder: boolean;
  detail: string;
}

export interface ConfigSnapshot {
  version: string;
  tools: ToolView[];
  log_level: string;
  log_levels: string[];
  catalog_mode: string;
  catalog_modes: string[];
  catalog_url_template: string;
  catalog_status: string;
  manifest_url: string;
  manifest_sources: string[];
  lua_paths: string[];
  lua_dir: string;
  owned_count: number;
  epoch: number;
  channel: string;
  note: string;
  managed: number[];
  managed_names: Record<string, string>;
}

export type ConfigIntent =
  | { kind: "set_tool"; id: string; on: boolean }
  | { kind: "set_log_level"; value: string }
  | { kind: "set_catalog_mode"; value: string }
  | { kind: "set_catalog_url_template"; value: string }
  | { kind: "set_manifest_url"; value: string }
  | { kind: "add_lua_path"; value: string }
  | { kind: "remove_lua_path"; value: string }
  | { kind: "refresh_app"; app_id: number }
  | { kind: "remove_app"; app_id: number };

declare global {
  interface Window {
    __SteamToolsIntents?: ConfigIntent[];
    __SteamToolsPanel?: { update: (snapshot: ConfigSnapshot) => void };
    __SteamToolsClose?: (() => void) | null;
    __SteamToolsClosed?: boolean;
    __SteamToolsWant?: boolean;
    __SteamToolsNavRow?: Element | null;
  }
}

export function emitIntent(intent: ConfigIntent): void {
  const queue = window.__SteamToolsIntents ?? (window.__SteamToolsIntents = []);
  queue.push(intent);
}

export const EMPTY_SNAPSHOT: ConfigSnapshot = {
  version: "?",
  tools: [],
  log_level: "info",
  log_levels: [],
  catalog_mode: "disabled",
  catalog_modes: [],
  catalog_url_template: "",
  catalog_status: "未配置",
  manifest_url: "opensteamtool",
  manifest_sources: [],
  lua_paths: [],
  lua_dir: "",
  owned_count: 0,
  epoch: 0,
  channel: "",
  note: "",
  managed: [],
  managed_names: {}
};
