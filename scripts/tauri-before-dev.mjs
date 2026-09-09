#!/usr/bin/env node
// Smart beforeDevCommand for `tauri dev`.
//
// Behavior:
// 1. Ensures the duckling-proxy sidecar binary exists (tauri's externalBin
//    check requires it before the Rust build starts); builds it if missing.
// 2. If something is already serving the frontend on :12341 (e.g. the
//    "Frontend (Next.js)" half of the VS Code "Run Full App" compound), exits
//    immediately so tauri can proceed without starting a second dev server.
// 3. Otherwise starts `pnpm dev` itself, waits until :12341 responds, then
//    exits 0. The Next process keeps running in the same console/process
//    group, so it dies together with the tauri dev session.

import { spawn, execSync } from "node:child_process";
import { existsSync } from "node:fs";
import net from "node:net";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(scriptDir, "..");
const isWindows = process.platform === "win32";
const PORT = 12341;
const HOST = "127.0.0.1";

function isPortUp() {
  return new Promise((resolvePromise) => {
    const socket = net.connect(PORT, HOST);
    socket.once("connect", () => {
      socket.destroy();
      resolvePromise(true);
    });
    socket.once("error", () => resolvePromise(false));
  });
}

async function waitForPort(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await isPortUp()) return true;
    await new Promise((r) => setTimeout(r, 300));
  }
  return false;
}

let target = "unknown";
try {
  const output = execSync("rustc -vV", { encoding: "utf-8" });
  const match = output.match(/host:\s*(.+)/);
  if (match) target = match[1].trim();
} catch {}

const exeSuffix = isWindows ? ".exe" : "";
const sidecarDest = join(
  projectRoot,
  "src-tauri",
  "binaries",
  `duckling-proxy-${target}${exeSuffix}`,
);

if (!existsSync(sidecarDest)) {
  console.log("[before-dev] sidecar missing, building duckling-proxy...");
  const copy = spawn(
    "node",
    ["src-tauri/copy-proxy-binary.mjs"],
    { stdio: "inherit", shell: isWindows, cwd: projectRoot },
  );
  const copyCode = await new Promise((resolvePromise) => {
    copy.on("exit", resolvePromise);
    copy.on("error", () => resolvePromise(1));
  });
  if (copyCode !== 0 || !existsSync(sidecarDest)) {
    console.error(
      `[before-dev] failed to build duckling-proxy sidecar (exit ${copyCode})`,
    );
    process.exit(1);
  }
} else {
  console.log("[before-dev] sidecar binary present, skipping build");
}

if (await isPortUp()) {
  console.log(
    "[before-dev] frontend already serving on :12341, skipping dev server start",
  );
  process.exit(0);
}

console.log("[before-dev] starting frontend dev server (pnpm dev)");
const next = spawn("pnpm", ["dev"], {
  stdio: "inherit",
  shell: isWindows,
  cwd: projectRoot,
});

let stopping = false;
function stopChild() {
  if (stopping) return;
  stopping = true;
  try {
    next.kill();
  } catch {}
}
process.on("SIGINT", stopChild);
process.on("SIGTERM", stopChild);
next.on("error", (err) => {
  console.error(`[before-dev] failed to start frontend: ${err.message}`);
  process.exit(1);
});

const up = await waitForPort(180_000);
if (!up) {
  console.error(
    "[before-dev] frontend did not become reachable on :12341 in time",
  );
  stopChild();
  process.exit(1);
}
console.log(
  "[before-dev] frontend is up on :12341; handing over to tauri (server keeps running)",
);
process.exit(0);
