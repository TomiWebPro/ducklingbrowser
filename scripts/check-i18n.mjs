#!/usr/bin/env node
/**
 * i18n consistency check.
 *
 * Guards against translation keys being referenced from the frontend but
 * missing from a locale file — the "raw key rendered to the user" bug class
 * (e.g. `pageTitle.keys` shown verbatim in the header when a new AppPage was
 * added without a matching `pageTitle.<page>` entry).
 *
 * Checks:
 * 1. Flattened key sets are identical across every locale (vs en.json):
 *    zero missing, zero extra.
 * 2. Every statically referenced `t("namespace.key")` key exists in every
 *    locale.
 * 3. Every dynamically interpolated `t(\`prefix.${...}\`)` key expands to
 *    existing keys: the prefix's value domain is derived from real source
 *    (AppPage union, Rust rollover stages, WEEKDAY_KEYS, SyncStatus unions),
 *    so a new page/status/stage missing its translations fails the check.
 *    Unknown prefixes fail loudly so new dynamic keys must be taught to the
 *    domain registry below instead of silently escaping coverage.
 */

import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, "..");
const LOCALES_DIR = path.join(ROOT, "src", "i18n", "locales");
const SRC_DIR = path.join(ROOT, "src");
const RUST_DIR = path.join(ROOT, "src-tauri", "src");

const failures = [];
let checks = 0;

function fail(message) {
  failures.push(message);
}

function readSources(dir, exts, acc = []) {
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) {
      readSources(full, exts, acc);
    } else if (exts.some((ext) => entry.endsWith(ext))) {
      acc.push(full);
    }
  }
  return acc;
}

function flatten(obj, prefix = "") {
  const out = new Set();
  for (const [key, value] of Object.entries(obj)) {
    const fullKey = prefix ? `${prefix}.${key}` : key;
    if (typeof value === "object" && value !== null) {
      for (const leaf of flatten(value, fullKey)) out.add(leaf);
    } else {
      out.add(fullKey);
    }
  }
  return out;
}

function loadLocales() {
  const locales = {};
  for (const file of readdirSync(LOCALES_DIR).filter((f) =>
    f.endsWith(".json"),
  )) {
    const name = file.replace(/\.json$/, "");
    const raw = JSON.parse(readFileSync(path.join(LOCALES_DIR, file), "utf8"));
    locales[name] = { raw, flat: flatten(raw) };
  }
  return locales;
}

function extractAppPages() {
  const src = readFileSync(
    path.join(SRC_DIR, "components", "rail-nav.tsx"),
    "utf8",
  );
  const match = src.match(
    /export type AppPage\s*=\s*((?:[^;]*"[a-zA-Z0-9_-]+"[^;]*)+);/,
  );
  if (!match)
    throw new Error("Could not parse AppPage union from rail-nav.tsx");
  return [...match[1].matchAll(/"([a-zA-Z0-9_-]+)"/g)].map((m) => m[1]);
}

function extractRolloverStages() {
  const src = readFileSync(path.join(RUST_DIR, "sync", "engine.rs"), "utf8");
  const stages = [...src.matchAll(/"stage":\s*"([a-zA-Z0-9_]+)"/g)].map(
    (m) => m[1],
  );
  if (stages.length === 0)
    throw new Error("Could not parse rollover stages from sync/engine.rs");
  return [...new Set(stages)];
}

function extractWeekdays() {
  const src = readFileSync(
    path.join(SRC_DIR, "components", "task-calendar.tsx"),
    "utf8",
  );
  const match = src.match(/const WEEKDAY_KEYS\s*=\s*\[([^\]]+)\]/);
  if (!match)
    throw new Error("Could not parse WEEKDAY_KEYS from task-calendar.tsx");
  return [...match[1].matchAll(/"([a-z]+)"/g)].map((m) => m[1]);
}

function extractSyncStatuses() {
  const statuses = new Set();
  for (const file of readSources(SRC_DIR, [".ts", ".tsx"])) {
    const src = readFileSync(file, "utf8");
    for (const match of src.matchAll(
      /type SyncStatus\s*=\s*((?:"[a-z]+"\s*\|\s*)+"[a-z]+")/g,
    )) {
      for (const literal of match[1].matchAll(/"([a-z]+)"/g))
        statuses.add(literal[1]);
    }
  }
  if (statuses.size === 0)
    throw new Error("Could not parse any SyncStatus union from src/");
  return [...statuses];
}

// Dynamic template prefixes (`t(\`prefix.${...}\`)`) mapped to their value
// domains, each derived from the actual source that produces the values.
const DYNAMIC_DOMAINS = {
  "pageTitle.": extractAppPages(),
  "encryption.rollover.stage.": extractRolloverStages(),
  "tasks.calendar.weekdays.": extractWeekdays(),
  "profileInfo.syncStatusValue.": extractSyncStatuses(),
};

function collectReferencedKeys() {
  const staticKeys = new Set();
  const dynamicPrefixes = new Set();
  for (const file of readSources(SRC_DIR, [".ts", ".tsx"])) {
    const src = readFileSync(file, "utf8");
    for (const match of src.matchAll(/\bt\(\s*["']([A-Za-z0-9_.]+)["']/g)) {
      staticKeys.add(match[1]);
    }
    for (const match of src.matchAll(/\bt\(\s*`([^`]*)\$\{/g)) {
      const prefix = match[1];
      if (!prefix) continue;
      const leadingDot = prefix.endsWith(".");
      if (leadingDot) {
        dynamicPrefixes.add(prefix);
      } else {
        const dot = prefix.lastIndexOf(".");
        if (dot !== -1) dynamicPrefixes.add(prefix.slice(0, dot + 1));
      }
    }
  }
  const expanded = new Set(staticKeys);
  for (const prefix of dynamicPrefixes) {
    const domain = DYNAMIC_DOMAINS[prefix];
    if (!domain) {
      fail(
        `t(\`${prefix}\${...}\`) has no registered value domain — add a source-derived ` +
          `domain to DYNAMIC_DOMAINS in scripts/check-i18n.mjs so its keys are covered.`,
      );
      continue;
    }
    for (const value of domain) {
      expanded.add(`${prefix}${value}`);
      checks++;
    }
  }
  return { staticKeys, dynamicPrefixes, expanded };
}

const locales = loadLocales();
const en = locales.en;
const { staticKeys, dynamicPrefixes } = collectReferencedKeys();

for (const [name, locale] of Object.entries(locales)) {
  for (const key of [...en.flat].sort()) {
    checks++;
    if (!locale.flat.has(key)) {
      fail(`[${name}] missing key vs en.json: ${key}`);
    }
  }
  for (const key of [...locale.flat].sort()) {
    checks++;
    if (!en.flat.has(key)) {
      fail(`[${name}] extra key vs en.json: ${key}`);
    }
  }
}

for (const key of [...staticKeys].sort()) {
  checks++;
  for (const [name, locale] of Object.entries(locales)) {
    if (!locale.flat.has(key)) {
      fail(
        `[${name}] static t("${key}") is referenced but missing from the locale`,
      );
    }
  }
}

for (const prefix of [...dynamicPrefixes].sort()) {
  const domain = DYNAMIC_DOMAINS[prefix];
  if (!domain) continue;
  for (const value of domain) {
    const key = `${prefix}${value}`;
    for (const [name, locale] of Object.entries(locales)) {
      checks++;
      if (!locale.flat.has(key)) {
        fail(
          `[${name}] dynamic key ${key} (from t(\`${prefix}\${...}\`)) is missing`,
        );
      }
    }
  }
}

const summary = [
  `Checked ${checks.toLocaleString()} key/locale pairs`,
  `${Object.keys(locales).length} locales`,
  `${staticKeys.size} static keys`,
  `${dynamicPrefixes.size} dynamic prefixes: ${[...dynamicPrefixes].sort().join(", ") || "none"}`,
];

if (failures.length > 0) {
  console.error(`\ni18n check FAILED (${failures.length} problem(s)):\n`);
  for (const f of failures) console.error(`  - ${f}`);
  console.error(`\n${summary.join(" | ")}\n`);
  process.exit(1);
}

console.log(`i18n check OK: ${summary.join(" | ")}`);
