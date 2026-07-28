import { build } from "esbuild";
import { readFile, stat, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const output = resolve(root, "embed/panel.iife.js");
const maxBytes = 64 * 1024;
const verify = process.argv.includes("--verify");
let previous = null;
try {
  previous = await readFile(output, "utf8");
} catch {
  // 首次构建没有旧产物, 由普通 build 生成.
}

await build({
  entryPoints: [resolve(root, "src/main.tsx")],
  bundle: true,
  format: "iife",
  platform: "browser",
  target: ["es2018"],
  minify: true,
  legalComments: "none",
  outfile: output,
  loader: { ".css": "text" },
  charset: "ascii",
  logLevel: "warning"
});

const size = (await stat(output)).size;
if (size > maxBytes) {
  throw new Error(`panel bundle is ${size} bytes, limit is ${maxBytes}`);
}

const source = await readFile(output, "utf8");
for (const marker of ["stt-panel", "__SteamToolsPanel", "__SteamToolsIntents", "__SteamToolsClose"]) {
  if (!source.includes(marker)) throw new Error(`bundle is missing ${marker}`);
}
for (const forbidden of ["fetch(", "XMLHttpRequest", "WebSocket", "window.open", "import(", "<script"]) {
  if (source.includes(forbidden)) throw new Error(`bundle contains forbidden ${forbidden}`);
}

const normalized = source.replace(/\s+$/u, "") + "\n";
if (verify && previous !== null && previous !== normalized) {
  throw new Error("embed/panel.iife.js is stale; run npm run build");
}
await writeFile(output, normalized, "utf8");
console.log(`panel.iife.js: ${size} bytes`);
