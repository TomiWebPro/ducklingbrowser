#!/usr/bin/env node
// Exit 0 when a real production frontend build exists, non-zero otherwise.
// Used by tauri.conf.json beforeBuildCommand to skip redundant `next build`s.
//
// What counts as "real": dist/_next/ (emitted by `next build` with static
// export). A bare dist/ or dist/index.html is NOT enough — build.rs
// materializes a stub index.html so bare `cargo build` works without a
// frontend, and bundling that stub would ship an empty app.
//
// All paths resolve from this script's location, so the hook works no matter
// which directory tauri spawns it in.
import { existsSync, statSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const marker = resolve(projectRoot, "dist", "_next");

let ok = false;
try {
  ok = existsSync(marker) && statSync(marker).isDirectory();
} catch {
  ok = false;
}
process.exit(ok ? 0 : 1);
