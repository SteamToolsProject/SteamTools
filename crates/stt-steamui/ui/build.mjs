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

// 归一化产物行尾: Windows checkout 可能把源文件转成 CRLF, esbuild
// 会把真实 CRLF 或字面 \r\n 转义带进产物; 统一 LF 再比较/写回,
// 避免环境性误报.
const normalizeEol = (text) =>
  text.replace(/\\r\\n/gu, "\\n").replace(/\r\n?/gu, "\n").replace(/\s+$/u, "") + "\n";
const normalized = normalizeEol(source);
const previousNorm = previous === null ? null : normalizeEol(previous);
if (verify && previousNorm !== null && previousNorm !== normalized) {
  // 定位首个差异, 便于区分环境性差异 (行尾/空白) 与真实产物漂移.
  let i = 0;
  const max = Math.min(previousNorm.length, normalized.length);
  while (i < max && previousNorm.charCodeAt(i) === normalized.charCodeAt(i)) i += 1;
  const prevChunk = previousNorm.slice(Math.max(0, i - 40), i + 40);
  const newChunk = normalized.slice(Math.max(0, i - 40), i + 40);
  throw new Error(
    `embed/panel.iife.js is stale; run npm run build ` +
      `(first diff at ${i}, prev=${previousNorm.length} new=${normalized.length})`
  );
}
await writeFile(output, normalized, "utf8");
console.log(`panel.iife.js: ${size} bytes`);
