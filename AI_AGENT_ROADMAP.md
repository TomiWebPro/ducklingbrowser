# AI Agent + AI Scheduled Tasks — Implementation Roadmap

Status: approved plan. M0–M8 implemented 2026-08-02 (M0 `4389c6a`, M1 `9f3e3e6`, M2 `23aadb8`, M3 `eedec53`, M4 `18e17fe`, M5 `0a9e44c`, M6 `74d6993`, M7 `52a58b8`, M8 `d8f1896`; all pushed). Native e2e suites cannot run on this machine (missing webdriver sibling repo) — verification is `cargo test --lib` + `pnpm lint`; e2e evidence files are maintained for CI.
When a session resumes: read this file first, then execute M1 onward in order.

---

## 1. Mission

Two new rail tabs in the Duckling browser app:

1. **"agent" tab** — natural-language chat where an AI uses the browser's own tools
   (CDP automation, profile/proxy ops) to do tasks. The agent proposes changes;
   they render as elegant confirm/decline cards; nothing is applied without
   explicit user confirmation.
2. **"tasks" tab** — calendar view for scheduling tasks (cron). Daily tasks run in a
   **random time window** that is **re-rolled every day + jitter** (anti-detection).
   Executed by the backend while the app sits in the tray (24/7 by default).

Plus a **"keys" tab**: BYOK (bring-your-own-key) management for AI providers.

---

## 2. Locked decisions (user-confirmed — do not re-litigate)

- **BYOK only. NO bundled local AI, NO Ollama, NO local model downloads.** App must
  be usable out-of-the-box with zero extra downloads (keys are user-supplied).
- Providers to support: **Anthropic, OpenAI, Groq, Google AI (Gemini), OpenRouter**.
- Keys encrypted at rest (reuse Argon2 + AES-256-GCM pattern, see §4.3).
- Per-conversation **model picker** in the chat; default = the key's saved model.
- **Delegation toggle** in chat: instead of an API key, run the task through an
  installed MCP agent CLI (e.g. opencode, claude-code — from the existing
  `mcp_integrations.rs` registry). When on, no API key needed.
- Execution model **C (hybrid)**: tasks run as **compiled MacroSteps** by default;
  each task has an escape hatch `mode: "live_agent"` that hands execution to an
  installed agent. User chose this over all-macro (A) and all-live (B).
- Calendar supports **month / week / day zoom**.
- Cron: **daily only** for v1. Random window re-rolled daily + jitter.
- **24/7 background ON by default**: new setting `keep_running_in_background`
  default `true`; closing the window hides to tray (app + scheduler keep running).
  Toggle exists in Settings. v1 = GUI-only runtime (scheduler lives in the main
  process). Standalone daemon/service = future work, NOT in scope now.
- "All confirm for now" — user will push back on anything they dislike; proceed
  with the open defaults in §2.1.

### 2.1 Open defaults — proceed unless user objects

- Rate limiting: scheduled jobs share the **same per-hour bucket** as manual/MCP
  automation (`automation_rate_limiter.rs`), with an **opt-in per-task checkbox
  default ON** (`same_bucket_rate_limit: true`).
- Chat v1 is **non-streaming** (single response after agent loop completes).
- No chat/task history persistence in v1 (in-memory session only).
- Missed-job policy: if a job comes due while the machine was asleep/closed, run it
  only if `now` is still inside the task's window; otherwise skip to next day.
- Jitter default: 30 minutes. Task timezone: per-task, from chrono-tz.
- In cron/live-agent context there is no human to approve cards — apply only cards
  flagged `reversible: true`; skip and report irreversible ones.
- Change cards are never auto-applied in the chat context.

---

## 3. Repo & environment ground rules

- Repo: `/home/tomi/development/ducklingbrouser`, branch `main` (commit `fdb2c92`).
- Origin: `https://TomiWebPro:github_pat_...@github.com/TomiWebPro/ducklingbrowser.git`
  — the PAT is embedded in `.git/config` by user choice: **use it, push normally,
  never touch/delete it**.
- `AI_AGENT_ROADMAP.md` lives at repo root; commit it with the M0 work.
- Toolchain: pnpm 11.10.0; `typos` installed at `/home/tomi/.cargo/bin/typos` (was
  missing from PATH — if a hook fails on `typos: command not found`, reinstall via
  `cargo install typos-cli`).
- Pre-commit hook (`pnpm exec lint-staged` via `.husky/pre-commit`) runs:
  - `biome check --fix` on staged `{js,jsx,ts,tsx,json,css}`
  - `cargo fmt --all` + `cargo clippy --all-targets --all-features -- -D warnings -D clippy::all` + `cargo test --lib` on staged `.rs`
  - `typos` on all staged
- **Known hook quirk:** `cargo fmt --all` formats the ENTIRE crate but lint-staged
  only stages matching files. If the hook leaves extra reformatted files unstaged,
  commit them separately (`git add -u && git commit`) — formatting-only, safe.

### 3.1 Convention traps (each blocks CI unless handled)

1. `src-tauri/src/lib.rs` `check_unused_commands` (~2512–2604): every command in
   `generate_handler!` must be referenced in the React `src/` OR listed in
   `non_frontend_commands` (current allowlist quoted at lib.rs:2516–2532). New UI
   commands are used by frontend → pass automatically.
2. `e2e/coverage-map.mjs` + `e2e/tests/coverage.test.mjs`: the map's command set
   must be **exactly equal** to commands parsed from `generate_handler!`. Adding a
   command to `invoke_handler` without updating the map **fails coverage.test.mjs
   immediately**. Every command needs an entry with a `suite` whose test file
   contains `invoke("cmd")` / `invokeError("cmd")` / `invokeContract(x, "cmd")`
   evidence. New suites: `e2e/tests/ai.test.mjs`, `e2e/tests/tasks.test.mjs`.
3. MCP `create_profile` accepts ONLY engine `"chromium"` (mcp_server.rs:2235–2240,
   schema enum at 626) — keep in lockstep; `api_server.rs` test
   `create_profile_browser_validation_matches_supported_engines` mirrors it.
4. `cargo test --lib` must stay green (357 tests, all passing).
5. e2e suite level semantics: `"integration" | "contract" | "host-mutating"`
   (host-mutating needs a `reason` string > 80 chars). Keep new commands
   `"integration"` in the `ai` / `tasks` suites.
6. **Translations (mandatory):** never write user-facing strings as raw English in
   JSX/toasts/labels — always `t("namespace.key")`, no 2-arg fallback form. Adding
   a key means adding it to EVERY file in `src/i18n/locales/` (en, es, fr, ja, ko,
   pt, ru, tr, vi, zh — enumerate the dir, don't trust the list). Use a one-shot
   Python script over `src/i18n/locales/*.json` to add/remove keys in lockstep;
   finish by diffing each locale's flattened key set against `en.json` (zero
   missing, zero extra).
7. **Backend error codes (mandatory):** user-facing Tauri-command errors must be
   JSON strings `{"code":"FOO_BAR","params":{...}}` + `BackendErrorCode` union +
   case in `src/lib/backend-errors.ts` + `backendErrors.fooBar` in every locale.
   Never `format!("Failed to ...")` as the error string.
8. **Theming:** no hardcoded Tailwind color classes (`text-red-500` etc.) — use
   theme vars (`bg-success`, `text-destructive`, `border-warning/50`, ...) from
   `src/lib/themes.ts`.
9. **e2e per area** (AGENTS.md table): new tabs/dialogs → `pnpm e2e:ui`;
   MCP/server contracts → `pnpm e2e:integrations`; CDP/automation → `pnpm e2e:browser`
   (requires `CHROMIUM_TEST_TOKEN` in env or local `.env` — without it, run
   `e2e:integrations` + note the gap). `e2e:network`/full need Docker +
   residential proxy URLs. `e2e` feature builds skip tray (`#[cfg(not(feature = "e2e"))]`).
10. **Sub-page dialog pattern:** reuse the exact `subPage` Tabs class strings from
    `account-page.tsx` / `proxy-management-dialog.tsx` (they're tuned); external
    triggers pass `initialTab` + `key={initialTab}` to force remount.
11. **Singletons:** if a global singleton of a struct exists, only use it inside a
    method while properly initializing it (per AGENTS.md).

---

## 4. Existing code anchors (verified 2026-08-02)

### 4.1 CDP plumbing to extract — `src-tauri/src/mcp_server.rs`

Section `// --- CDP utility methods for browser interaction ---` (3925–4321):

| Function | Lines | Signature |
|---|---|---|
| `get_cdp_port_for_profile` | 3927 | `async fn(&self, profile: &BrowserProfile) -> Result<u16, McpError>` — retries 10×1s, `ChromiumManager::instance().get_cdp_port(profile_path)`, only when `profile.browser == "chromium"` (3937) |
| `get_cdp_ws_url` | 3958 | `async fn(&self, port: u16) -> Result<String, McpError>` — GET `http://127.0.0.1:{port}/json` up to 15 attempts, first `"type":"page"` target → `webSocketDebuggerUrl` |
| `send_cdp` | 4003 | `async fn(&self, ws_url, method, params) -> Result<Value, McpError>` — `tokio_tungstenite::connect_async` per call, id 1, read until id match |
| `send_human_keystrokes` | 4067 | `async fn(&self, ws_url, text, wpm) -> Result<(), McpError>` — MarkovTyper + `Input.dispatchKeyEvent` keyDown/up pairs |
| `send_cdp_and_wait_for_load` | 4192 | `async fn(&self, ws_url, method, params, timeout_secs) -> Result<Value, McpError>` — `Page.enable`, cmd, wait `Page.loadEventFired`, `Page.disable` |
| `get_running_profile` | 4323 | `fn(&self, profile_id) -> Result<BrowserProfile, McpError>` |

Port mapping: `chromium_manager.rs` — each launch gets a random free port
(`--remote-debugging-port=`), stored in-memory in `ChromiumInstance { id, process_id,
profile_path, url, cdp_port }` (68–74); `get_cdp_port(profile_path)` (920–937) matches
by canonicalized profile path; recovery path re-reads process args (1066–1086).

**IMPORTANT:** there is NO `wait_for_selector` today. Interaction is index-based:
`get_interactive_elements` → `click_by_index` / `type_by_index` (tools at
mcp_server.rs:1550/1568/1586). M0 must preserve this MCP tool behavior exactly;
`wait_for_selector` is a NEW helper we add (polling `Runtime.evaluate`).

MCP server facts: HTTP on `127.0.0.1`, `DEFAULT_MCP_PORT=51080` (107), start 163–231,
auth path-token or Bearer constant-time (291–330), automation rate-limit → 429 +
Retry-After (437–452, `is_automation_tool_call` 459). 60 tools total
(`get_tools` 515–1620, dispatch 1764–1855). Browser-interaction tools:
`navigate` (1384), `screenshot` (1402), `evaluate_javascript` (1430),
`click_element` (1458), `type_text` (1476), `get_page_content` (1510),
`get_page_info` (1535), `get_interactive_elements` (1550), `click_by_index` (1568),
`type_by_index` (1586). Handlers `handle_*` at 4358–5005, all
`async fn(&self, arguments: &Value) -> Result<Value, McpError>`.

### 4.2 Agent registry — `src-tauri/src/mcp_integrations.rs`

`AGENT_SPECS` (222–321): 14 agents (`claude-desktop`, `claude-code`, `cursor`,
`vscode`, `zed`, `cline-cli`, `cline`, `codex`, `gemini-cli`, `github-copilot-cli`,
`goose`, `antigravity`, `opencode`, `mcporter`). `McpAgentInfo { id, display_name,
category, connected, detected }` (42–52). `list_agents_with_status` (552–570).
All install/remove is config-file writing — **no process spawning exists anywhere**.
The live-agent delegation path (M4/M6) will be the first `tokio::process::Command`
spawn in the codebase (CLI agents only: opencode, claude-code, gemini-cli, goose,
codex, cline-cli).

### 4.3 Token encryption pattern — `src-tauri/src/settings_manager.rs`

Argon2 + AES-256-GCM, versioned binary file:
- `get_vault_password()` (187–189) = `env!("DUCKLING_BROWSER_VAULT_PASSWORD")`
  (build-time env injected by `build.rs` 39–43; hardcoded fallback
  `"ducklingbrowser-api-vault-password"`).
- Layout (see `store_mcp_token` 403–448): header `DBMCP` (5 bytes) + version byte
  (2) + salt-len byte + salt (16 bytes, b64-encoded) + 12-byte nonce + u32-LE
  ciphertext-len + ciphertext. Salt via `argon2.hash_password(...)`, first 32 bytes
  of hash = AES-256-GCM key. `restrict_to_owner` after write.
- `get_mcp_token` (450–531) validates header/version, returns None on mismatch.
- Settings JSON: `app_settings.json` in `app_dirs::settings_dir()` (117–123);
  `load_settings` (129–147) defaults on unparsable; `save_settings` plain
  write. `AppSettings` struct 28–66, `Default` 82–104. Tokens never persisted in
  JSON (stripped at 803–805, overlaid from `.dat` at 708–726).
- **AI keys:** new `ai_keys.dat` (header `DBKEY`), same layout. **Preferred:** one
  file holding an encrypted JSON array of all keys (simpler list op). Refactor
  option: extract `pub(crate)` vault helpers from settings_manager.rs into a small
  `vault.rs` (encrypt_to_file/decrypt_file with header param) and re-use from both
  — do this carefully, keep settings tests green (token roundtrips are covered).

### 4.4 Rate limiter — `src-tauri/src/automation_rate_limiter.rs`

`RateLimitOutcome { Unlimited, Allowed { remaining }, Limited { retry_after_secs } }`
(9–14); `check_automation_rate_limit() -> RateLimitOutcome` (62–71); single
per-hour rolling window keyed by identity string from `CLOUD_AUTH.automation_rate_limit()`
(63–65); `requests_per_hour == 0` → Unlimited, no consume (23–25); the check IS the
consume (push `Instant::now()`, 22–56). In-memory, not persisted. Consumers: REST
middleware api_server.rs 720–735, MCP 437–452. Scheduled jobs use the same function
when `same_bucket_rate_limit: true`.

### 4.5 Frontend wiring

- `src/components/rail-nav.tsx`: `AppPage` union (24–34): profiles, proxies,
  extensions, groups, vpns, settings, integrations, account, import, shortcuts.
  `TOP_ITEMS` RailItems (191–211), `MORE_ITEMS` (213–226). Settings = fixed bottom
  button (360–381). To add a tab: (1) union value, (2) TOP_ITEMS/MORE_ITEMS entry,
  (3) `case` in `handleRailNavigate` switch (page.tsx:373–407), (4) dialog state +
  conditional render (page.tsx:1601–1763 pattern — each dialog close resets
  `setCurrentPage("profiles")`), (5) i18n keys (`rail.*` namespace,
  `src/i18n/locales/*.json` — update ALL locale files).
- `src/app/page.tsx` render branches: profiles table 1601–1659, shortcuts 1661–1670,
  sub-page dialogs 1672–1763 (settings 1672, integrations 1688, proxy/vpns 1700,
  groups 1712, extensions 1724, import 1737, account 1749). `subPage={...}` prop
  pattern.
- `src/components/close-confirm-dialog.tsx`: listens `close-confirm-requested`
  (22–30), Minimize → `invoke("hide_to_tray")` (50–57), Quit → `invoke("confirm_quit")`
  (59–66). Rust: `confirm_quit` (lib.rs 1289–1294), `hide_to_tray` (1296–1303,
  hides "main" webview), `show_main_window` (1305–1312), `update_tray_menu`
  (1314–1340), `setup_system_tray` (1342–1408, `#[cfg(not(feature = "e2e"))]`,
  id "main", items tray_show/tray_quit, tray-quit sets QUIT_CONFIRMED + exit).
  Close-requested event emitted ~1498–1566.
- `src/components/integrations-dialog.tsx`: two-tab (`api`/`mcp`) sub-page; loads
  `get_app_settings`, `get_api_server_status`, `get_mcp_config`,
  `get_mcp_server_status`, `list_mcp_agents` (128–189); MCP toggle → `start_mcp_server`/
  `stop_mcp_server` + `save_app_settings` (223–252); Connect → `add_mcp_to_agent({agentId})`,
  Disconnect → `remove_mcp_from_agent` (263–295); gated on `useBrowserTerms()` (126/547).
  Reuse this pattern for the tasks tab's agent picker.
- Naming conventions (`src/components/`): flat kebab-case top level; sub-page
  dialogs `*-dialog.tsx` (integrations-dialog, settings-dialog, ...); full pages
  `*-page.tsx` (account-page, shortcuts-page); UI primitives in `ui/` (32 shadcn
  primitives incl. button, dialog, tooltip, animated-tabs). New files:
  `ai-keys-dialog.tsx`, `agent-chat-dialog.tsx`, `change-card.tsx`,
  `scheduled-tasks-dialog.tsx`, `task-calendar.tsx`.

### 4.6 Dependencies (src-tauri/Cargo.toml)

- HTTP: `reqwest 0.13` (native-tls, json, stream, socks, http2, system-proxy) —
  use for LLM calls. NO openai/anthropic SDK crates — hand-rolled clients.
- WS: `tokio-tungstenite 0.29` (native-tls). Crypto: `aes-gcm 0.11`, `argon2 0.5`,
  `rand 0.10.2`, `blake3 1`, `ring 0.17`. Time: `chrono 0.4` (+serde), `chrono-tz 0.10`.
- **No cron/scheduler crate** — implement scheduling with tokio + chrono. Do NOT add
  a dependency unless needed; hand-rolled daily-window math is ~100 lines.
- Frontend: NO date/calendar lib (no date-fns/dayjs/react-day-picker) — hand-rolled
  calendar grid (Tailwind v4 + lucide-react icons). UI libs present: radix,
  sonner (toasts), tanstack table, i18next.

---

## 5. Target architecture (new files)

```
src-tauri/src/
  cdp_session.rs      M0  persistent CDP session (extracted from mcp_server.rs)
  scheduler.rs        M1  TaskDefinition + daily-window math + JobRunner
  vault.rs            M2  (optional refactor) shared encrypt/decrypt helpers
  ai_keys.rs          M2  AI key CRUD + test-key probes + masking
  llm.rs              M4  per-provider request builders (OpenAI-compat/Anthropic/Google)
  agent_engine.rs     M4  tool-calling loop, ChangeCard model, delegation spawn
  macro_step.rs       M6  MacroStep enum (serde tag="op")
  task_runner.rs      M6  executes TaskDefinition via CdpSession
src/
  components/ai-keys-dialog.tsx        M3
  components/agent-chat-dialog.tsx     M4
  components/change-card.tsx           M4
  components/scheduled-tasks-dialog.tsx M5
  components/task-calendar.tsx         M5
e2e/tests/ai.test.mjs                  M3/M4  (evidence for ai_keys_*, agent_*)
e2e/tests/tasks.test.mjs               M5     (evidence for scheduler_*)
```

Data models:

```rust
// scheduler.rs
struct TaskDefinition {
  id: String,                       // uuid v4
  name: String,
  description: Option<String>,
  mode: TaskMode,                   // "macro" | "live_agent" (serde kebab)
  agent_id: Option<String>,         // live_agent: registry agent id (CLI-only)
  prompt: Option<String>,           // live_agent: instruction text
  steps: Vec<MacroStep>,            // macro mode
  schedule: Schedule,
  same_bucket_rate_limit: bool,     // default true
  enabled: bool,                    // default true
  created_at: String, updated_at: String,       // RFC3339
  next_run_at: Option<String>,      // RFC3339, computed
  last_run_at: Option<String>, last_run_status: Option<String>,
  last_run_error: Option<String>, last_run_duration_ms: Option<u64>,
}
struct Schedule {
  window_start: String,             // "HH:MM" (task-local time)
  window_end: String,               // "HH:MM"
  timezone: String,                 // chrono-tz name, default "UTC"
  jitter_minutes: u32,              // default 30
  randomize_daily: bool,            // default true  (re-roll daily)
}
// ai_keys.rs
enum AiProvider { anthropic, openai, groq, google, openrouter }  // serde kebab
struct AiKey {
  id: String, provider: AiProvider, name: String, model: String,
  created_at: String,
}
// macro_step.rs
enum MacroStep {
  Navigate { url: String },
  WaitSelector { selector: String, timeout_ms: u64 },
  Click { selector: Option<String>, index: Option<u32> },
  Type { selector: Option<String>, index: Option<u32>, text: String },
  Evaluate { expression: String },
  Screenshot,
  Extract { expression: String, key: String },
  SaveProfileField { path: String, value: serde_json::Value },
}
// agent_engine.rs
struct ChangeCard {
  id: String,
  kind: ChangeKind,                 // profile_update | navigate | run_browser |
                                    // proxy | fingerprint | custom
  title: String, description: String,
  diff: serde_json::Value,          // before/after preview
  reversible: bool,
}
```

---

## 6. Milestones

### M0 — CdpSession refactor (foundation) — ✅ DONE 2026-08-02

Goal: extract CDP plumbing so MCP and the scheduler/agent share one client.

Implemented:
- `src-tauri/src/cdp_session.rs` created: `CdpError { code, message }`,
  `CdpSession` unit struct (derive Default) with the six primitives ported
  VERBATIM from mcp_server.rs (`get_cdp_port_for_profile`, `get_cdp_ws_url`,
  `resolve_ws_url`, `send_cdp`, `send_human_keystrokes`,
  `send_cdp_and_wait_for_load`, `get_running_profile`) plus new
  `wait_for_selector(ws_url, selector, timeout_ms)` and
  `#[allow(dead_code)]` `CdpSessionTrait` (resolve_ws_url / navigate /
  evaluate / screenshot / wait_for_selector) for M6 testability.
- `mcp_server.rs`: the six methods are now one-line delegations to
  `CdpSession::new()`; `impl From<CdpError> for McpError` added. All 9
  `handle_*` browser-interaction handlers untouched (zero behavior change).
- `lib.rs`: `mod cdp_session;` declared (alphabetical, after browser_version_manager).
- Tests: `selector_expression_escapes_quotes`, `cdp_error_carries_code_and_message`.
- DEVIATION from plan: **no persistent/keep-alive WebSocket** — per-call
  connections preserved verbatim (the `Page.enable` id 1/2/3 + `loadEventFired`
  logic depends on fresh connections; persistence deferred, add only if M6
  executor needs it).
- Verification: `cargo clippy --all-targets --all-features -- -D warnings -D clippy::all`
  clean; `cargo test --lib` 357 pass. Native e2e UNAVAILABLE on this machine:
  `e2e/run.mjs` requires sibling repo `/home/tomi/development/tauri-cross-platform-webdriver`
  (private test-driver, per AGENTS.md) which is absent, and `e2e:browser` also
  needs `CHROMIUM_TEST_TOKEN`. MCP behavior change is a verbatim port; low risk.
- Commit: `4389c6a`-style normal commit: "Extract CDP session into cdp_session.rs (M0)"
  — included this roadmap file.

### M1 — Scheduler core (backend)

Goal: daily-window scheduling math + JobRunner + persistence + commands.

1. `src-tauri/src/scheduler.rs`:
   - `TaskDefinition` + `Schedule` (models above), `SchedulerStore` (unit-struct
     singleton like SettingsManager) with `Mutex<Vec<TaskDefinition>>`, persistence
     to `scheduler_tasks.json` in `app_dirs::settings_dir()` (plain JSON; not
     secret — no encryption).
   - Pure math functions (unit-testable, no tokio):
     - `fn daily_target(&self, date: NaiveDate, seed: &str) -> NaiveDateTime` —
       uniform random `[window_start, window_end)` in task timezone, deterministic
       per (task_id, date) so reloads don't move it within the same day.
     - `fn apply_jitter(t, jitter_minutes, seed)` — ±jitter around target.
     - `fn compute_next_run(task, now) -> DateTime<FixedOffset>` — day rollover,
       DST-safe via chrono_tz `with_ymd_and_hms`, past-window → next day.
   - `JobRunner`: `tokio::spawn` loop, tick every 30s (started in Tauri `setup`
     from lib.rs); due tasks → spawn per-task guard (`HashMap<String, JoinHandle>`,
     no double-run); calls `task_runner::run_task` (M6 stub for now — M1 wires the
     loop, M6 fills execution).
2. Settings: add `keep_running_in_background: bool = true` to `AppSettings`
   (struct + Default, settings_manager.rs:28–104). Scheduler runs whenever the app
   process is alive (independent of window).
3. Commands (registered in `generate_handler!`):
   `scheduler_list`, `scheduler_save` (create/update by id),
   `scheduler_delete`, `scheduler_set_enabled`. Frontend uses them → no allowlist.
4. Tests: window bounds, determinism (same seed → same target), jitter range,
   day rollover, past-window skip, timezone conversion, enabled/disabled.
5. **Immediately** add coverage-map group `scheduledTasks` (suite `tasks`) + create
   `e2e/tests/tasks.test.mjs` with `invoke()` evidence for all 4 commands (create →
   list → set_enabled → delete cycle; tasks with a far-future window so nothing runs).
   Coverage test is exact-equality — commit map+evidence in the SAME commit as the
   commands or CI breaks.
6. Commit: `Add daily-window scheduler core and scheduler commands (M1)`.

### M2 — AI keys backend

1. `src-tauri/src/ai_keys.rs`:
   - `AiProvider` enum + `AiKey` model. Masking: `fn mask(key: &str) -> String`
     (first 3 + `…` + last 4, "sk-***abc").
   - Vault: `ai_keys.dat` in settings dir, header `DBKEY`, version 2, same Argon2
     + AES-256-GCM layout as `store_mcp_token` (settings_manager.rs:403–448).
     Preferred: encrypt the whole JSON array in one file. If extracting shared
     helpers into `vault.rs`, keep settings_manager token tests green.
   - Never log or return decrypted keys to the frontend (list returns masks only).
2. Commands: `ai_keys_list` (masked), `ai_keys_save {provider,name,model,key}`,
   `ai_keys_delete {id}`, `ai_keys_test {provider, model, key?}` (uses saved key
   when `key` absent).
   - Test-key probes (reqwest 0.13, short timeouts ~10s):
     - openai: `GET https://api.openai.com/v1/models` (Bearer)
     - groq: `GET https://api.groq.com/openai/v1/models`
     - openrouter: `GET https://openrouter.ai/api/v1/models`
     - anthropic: `POST https://api.anthropic.com/v1/messages`
       (`{model, max_tokens:1, messages:[{role:"user",content:"ping"}]}`,
       headers `x-api-key` + `anthropic-version: 2023-06-01`)
     - google: `GET https://generativelanguage.googleapis.com/v1beta/models?key=...`
   - Return structured `{ok: bool, detail: String}`; auth failure vs network
     failure distinguished in `detail`.
3. Tests: mask format, encrypt/decrypt roundtrip, invalid-version file → None.
4. Coverage: group `aiKeys` (suite `ai`); `e2e/tests/ai.test.mjs` evidence:
   list (empty), save (roundtrip via list mask), delete, `ai_keys_test` with an
   obviously-invalid key asserting an error result (never require network).
5. Commit: `Add encrypted AI key store and test-key probes (M2)`.

### M3 — AI Keys tab UI

1. `rail-nav.tsx`: add `"keys"` to `AppPage`; TOP_ITEMS entry
   (`LuKeyRound`, labelKey `rail.keys`); i18n keys in ALL locale files
   (`rail.keys`, `keys.title`, form labels...).
2. `page.tsx`: `case "keys"` in `handleRailNavigate` (373–407) → open keys dialog;
   render branch `ai-keys-dialog` with `subPage={currentPage === "keys"}`.
3. `src/components/ai-keys-dialog.tsx` (follow integrations-dialog patterns):
   list of saved keys (provider badge, name, model, masked key, show/hide eye,
   delete with confirm); add form (provider select, name, model, key password
   input, Test button with spinner → result toast via sonner, Save).
4. Verify: `pnpm lint`, e2e `ai` suite, unused-commands test (frontend uses them).
5. Commit: `Add AI keys tab UI (M3)`.

### M4 — Agent chat tab (backend + UI)

1. `src-tauri/src/llm.rs`:
   - `struct LlmClient { provider, api_key, model }`; `async fn chat(&self,
     messages: Vec<ChatMessage>, tools: Option<Vec<ToolSpec>>) -> Result<LlmMessage>`
     — non-streaming v1.
   - Request builders per provider: OpenAI-compatible `POST /chat/completions`
     (openai, groq, openrouter), Anthropic `POST /v1/messages` (+tools
     `input_schema`), Google `POST generateContent`. Timeout ~120s. Normalize
     tool-call/assistant content into one internal message shape.
   - Unit tests for request bodies (no network): serde shapes per provider.
2. `src-tauri/src/agent_engine.rs`:
   - `ChangeCard` model (above). Tool registry: expose the MCP browser-interaction
     tool set (navigate, screenshot, evaluate_javascript, click_by_index,
     type_by_index, get_page_content, get_page_info, get_interactive_elements,
     list_profiles, get_profile) with JSON schemas mirrored from mcp_server.rs
     `get_tools` — agent sees the same tools the MCP server exposes.
   - Tool-calling loop: LLM → tool_call → execute (read-only tools run
     immediately via CdpSession; mutating ops → record as proposed ChangeCard,
     do NOT apply) → results back to LLM → final `{ reply, cards }`.
     Cap iterations (e.g. 20); structured exit (ask model for JSON summary).
   - `agent_chat { key_id: Option<String>, model: Option<String>, message: String,
     use_agent: Option<String> }`; `agent_chat_confirm { card_ids }`,
     `agent_chat_decline { card_ids }`. Confirm applies via CdpSession + profile
     save commands (reuse existing manager APIs from mcp_server handlers).
   - Delegation path: `use_agent` set → spawn CLI via `tokio::process::Command`
     (opencode / claude-code / gemini-cli / goose / codex / cline-cli from
     `AGENT_SPECS` — first spawn in codebase; follow their CLI usage flags, run
     non-interactive with the prompt piped), parse trailing JSON block
     `{reply, cards}`. Model picker values come from the selected key; when
     `use_agent` set, key not required.
3. UI: `agent-chat-dialog.tsx` — message list, input, send, spinner while running,
   `change-card.tsx` renders Confirm/Decline buttons per card (feedback via
   sonner); header: provider+model picker (from ai_keys_list) + delegation toggle.
   Empty state: no keys → CTA button to keys tab.
4. Tests: request-builder units, ChangeCard serde roundtrip, tool-loop iteration
   cap (mock LLM via trait `LlmProvider` — keep loop logic testable without network).
5. Coverage: extend suite `ai`: `agent_chat`, `agent_chat_confirm`,
   `agent_chat_decline` evidence (e2e calls them with no keys configured →
   assert structured error; evidence presence is what matters).
6. Commit: `Add agent chat with change-confirmation cards (M4)`.

### M5 — Tasks tab UI (calendar)

1. `rail-nav.tsx`: add `"tasks"` to `AppPage` (TOP_ITEMS, `LuCalendarDays`,
   `rail.tasks`); i18n keys.
2. `page.tsx`: `case "tasks"` + render branch `scheduled-tasks-dialog`.
3. `src/components/scheduled-tasks-dialog.tsx`:
   - Task list: name, next run (localized), last run status (green/red dot),
     enabled switch, edit/delete.
   - Create/edit form: name, description; agent picker from `list_mcp_agents`
     (filter CLI agents) → `mode: "live_agent"` + prompt textarea, or macro mode
     (steps editor v2 — v1: prompt-only stored but mode macro needs steps; keep
     v1 honest: if steps empty and no agent → validation error).
   - Schedule: daily toggle, window start/end (`<input type="time">` — no dep),
     timezone select (static list of ~20 chrono-tz zones), jitter minutes, 
     `randomize_daily`, `same_bucket_rate_limit` checkbox (default ON), enabled.
   - `src/components/task-calendar.tsx`: month/week/day zoom buttons; hand-rolled
     month grid (first-day offset, Sunday-first, Tailwind), day view draws the
     task window band; click day → pre-fill create form. Follow `animated-tabs.tsx`
     for zoom control styling.
4. Commands from M1 are frontend-used → unused-commands test passes; coverage
   evidence already in `tasks.test.mjs`.
5. Verify: `pnpm lint`, e2e `tasks` suite, full `pnpm e2e:smoke`.
6. Commit: `Add scheduled tasks tab with calendar (M5)`.

### M6 — Execution engine

1. `src-tauri/src/macro_step.rs`: `MacroStep` enum (serde `tag="op"`, fields
   above) + roundtrip tests.
2. `src-tauri/src/task_runner.rs`: `run_task(task, &mut impl CdpSessionTrait)`
   — macro mode: execute steps sequentially (Navigate, WaitSelector, Click/Type by
   index or selector, Evaluate, Extract, Screenshot, SaveProfileField via profile
   manager); per-step timeout; `WaitSelector` miss = step error (configurable
   later); record last_run_* on the TaskDefinition; live_agent mode: reuse
   agent_engine delegation with auto-apply of `reversible` cards only.
3. Rate limiting: when `same_bucket_rate_limit`, call
   `check_automation_rate_limit()` first; `Limited` → schedule retry after
   `retry_after_secs` (or skip run and log — choose retry-once-then-skip).
4. Wire `JobRunner` (M1) → `task_runner`. Idempotency: never run the same task
   twice within its window.
5. Tests: MacroStep roundtrip; executor with `FakeCdpSession` (navigate/click/
   evaluate recording); rate-limited path.
6. Commit: `Add macro-step executor and wire scheduler execution (M6)`.

### M7 — 24/7 background

1. Settings dialog: add `keep_running_in_background` toggle (default true).
2. `close-confirm-dialog.tsx`: when setting is true, close-requested →
   `hide_to_tray` directly (no dialog); else current confirm behavior. Read
   setting via `get_app_settings` (already available pattern).
3. Confirm scheduler keeps ticking with window hidden (it lives in the main
   process — verify manually: `pnpm tauri dev`, close window, task runs, tray
   reopens window via `show_main_window`).
4. NOT in scope: standalone daemon/service, autostart. Note as future work.
5. Commit: `Run in background 24/7 by default with hide-to-tray (M7)`.
   (e2e: tray hidden under `e2e` feature — manual verification only, note in PR.)

### M8 — Polish & hardening

- Edge cases: missed runs after sleep (policy §2.1), DST/timezone changes
  (recompute `next_run_at` on save and on load), duplicate-run guard, key
  rotation, model/keys deleted while chat open, empty states + error toasts
  (sonner), calendar a11y basics.
- Sweep: `pnpm lint`, `cargo test --lib`, `pnpm e2e:smoke`, `pnpm e2e --suite=full`
  (if feasible), unused-commands + coverage exact-equality, typos, fmt.
- Commit: `Polish AI agent and scheduler edge cases (M8)`.

---

## 7. New Tauri commands + coverage plan

| Command | Milestone | Frontend use | coverage-map group / suite |
|---|---|---|---|
| `scheduler_list` | M1 | tasks tab | `scheduledTasks` / `tasks` |
| `scheduler_save` | M1 | tasks tab | `scheduledTasks` / `tasks` |
| `scheduler_delete` | M1 | tasks tab | `scheduledTasks` / `tasks` |
| `scheduler_set_enabled` | M1 | tasks tab | `scheduledTasks` / `tasks` |
| `ai_keys_list` | M2 | keys tab | `aiKeys` / `ai` |
| `ai_keys_save` | M2 | keys tab | `aiKeys` / `ai` |
| `ai_keys_delete` | M2 | keys tab | `aiKeys` / `ai` |
| `ai_keys_test` | M2 | keys tab | `aiKeys` / `ai` |
| `agent_chat` | M4 | agent tab | `aiAgent` / `ai` |
| `agent_chat_confirm` | M4 | agent tab | `aiAgent` / `ai` |
| `agent_chat_decline` | M4 | agent tab | `aiAgent` / `ai` |

Rule: coverage-map additions + e2e `invoke()` evidence go in the SAME commit as
the commands (exact-equality test breaks otherwise). `e2e/coverage-map.mjs` format:
`{ groupName: { suite, level: "integration", commands: [...] } }`.

---

## 8. Verification playbook (before every commit)

```bash
cd /home/tomi/development/ducklingbrouser
cd src-tauri && cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings -D clippy::all && cargo test --lib
cd ..
pnpm lint                       # lint:js + lint:rust + lint:spell (typos)
pnpm e2e:smoke                  # fast suite; run before feature commits
git add -A && git commit        # hook re-runs everything
git push origin main            # PAT remote — just push, never edit .git/config
```

If the hook leaves unstaged `cargo fmt` drift in unrelated files: commit them
separately with `git add -u` (formatting-only).

---

## 9. First actions on resume

1. M1: scheduler core — pure math first (`compute_next_run`, jitter, rollover),
   then `JobRunner`, persistence, settings flag, then the 4 `scheduler_*`
   commands + coverage-map group `scheduledTasks` + `e2e/tests/tasks.test.mjs`
   evidence (same commit as the commands).
2. Commit M1, then M2 (AI keys vault + probes + commands + `ai` suite evidence).
3. M3 keys tab UI → M4 agent chat (llm.rs + agent_engine.rs + dialog) → M5 tasks
   tab calendar → M6 executor (macro_step.rs + task_runner.rs + CdpSessionTrait)
   → M7 background → M8 polish.
