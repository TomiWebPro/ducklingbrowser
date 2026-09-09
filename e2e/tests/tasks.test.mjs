import assert from "node:assert/strict";
import test from "node:test";
import { withApp } from "../lib/app.mjs";

test("scheduled task CRUD through Tauri commands", async () => {
  await withApp("tasks-crud", async (app) => {
    const initial = await app.invoke("scheduler_list");
    assert.ok(Array.isArray(initial));

    const saved = await app.invoke("scheduler_save", {
      task: {
        id: "",
        name: "Nightly refresh",
        description: "Automated nightly refresh",
        mode: "macro",
        profile_id: null,
        agent_id: null,
        prompt: null,
        steps: [],
        schedule: {
          window_start: "02:00",
          window_end: "04:00",
          timezone: "UTC",
          jitter_minutes: 30,
          randomize_daily: true,
        },
        same_bucket_rate_limit: true,
        enabled: true,
      },
    });
    assert.ok(saved.id, "save should assign an id");
    assert.ok(saved.next_run_at, "save should compute next_run_at");
    assert.equal(saved.name, "Nightly refresh");

    const listed = await app.invoke("scheduler_list");
    assert.ok(listed.some((t) => t.id === saved.id));

    const disabled = await app.invoke("scheduler_set_enabled", {
      id: saved.id,
      enabled: false,
    });
    assert.equal(disabled.enabled, false);
    assert.equal(disabled.next_run_at, null);

    await app.invoke("scheduler_delete", { id: saved.id });
    const after = await app.invoke("scheduler_list");
    assert.ok(!after.some((t) => t.id === saved.id));
  });
});

test("scheduler rejects invalid tasks", async () => {
  await withApp("tasks-validation", async (app) => {
    await app.invokeError("scheduler_save", {
      task: {
        id: "",
        name: "",
        mode: "macro",
        steps: [],
        schedule: {
          window_start: "02:00",
          window_end: "04:00",
          timezone: "UTC",
          jitter_minutes: 30,
          randomize_daily: true,
        },
        same_bucket_rate_limit: true,
        enabled: true,
      },
    });
    await app.invokeError("scheduler_delete", { id: "missing-task-id" });
  });
});

test("agent_browser tasks validate profile, prompt, tools, and steps", async () => {
  await withApp("tasks-agent-browser", async (app) => {
    const base = {
      id: "",
      name: "Browser agent task",
      description: null,
      mode: "agent_browser",
      steps: [],
      schedule: {
        window_start: "02:00",
        window_end: "04:00",
        timezone: "UTC",
        jitter_minutes: 30,
        randomize_daily: true,
      },
      same_bucket_rate_limit: true,
      enabled: true,
    };
    // Missing profile and prompt.
    await app.invokeError("scheduler_save", {
      task: { ...base, profile_id: null, prompt: null },
    });
    // Missing prompt.
    await app.invokeError("scheduler_save", {
      task: { ...base, profile_id: "some-profile", prompt: "   " },
    });
    // Disallowed tool.
    await app.invokeError("scheduler_save", {
      task: {
        ...base,
        profile_id: "some-profile",
        prompt: "Do it",
        allowed_tools: ["delete_profile"],
      },
    });
    // Bad step budget.
    await app.invokeError("scheduler_save", {
      task: {
        ...base,
        profile_id: "some-profile",
        prompt: "Do it",
        max_steps: 0,
      },
    });

    const saved = await app.invoke("scheduler_save", {
      task: {
        ...base,
        profile_id: "some-profile",
        prompt: "Open the dashboard",
        allowed_tools: ["navigate", "drag", "get_downloads"],
        download_dir_override: "reports",
        max_steps: 10,
      },
    });
    assert.ok(saved.id, "valid agent_browser task should save");
    assert.deepEqual(saved.allowed_tools, [
      "navigate",
      "drag",
      "get_downloads",
    ]);
    assert.equal(saved.max_steps, 10);

    await app.invoke("scheduler_delete", { id: saved.id });
  });
});

test("scheduler_run_now reports run outcomes", async () => {
  await withApp("tasks-run-now", async (app) => {
    const saved = await app.invoke("scheduler_save", {
      task: {
        id: "",
        name: "Manual run probe",
        mode: "macro",
        profile_id: null,
        agent_id: null,
        prompt: null,
        steps: [],
        schedule: {
          window_start: "02:00",
          window_end: "04:00",
          timezone: "UTC",
          jitter_minutes: 30,
          randomize_daily: true,
        },
        same_bucket_rate_limit: true,
        enabled: true,
      },
    });

    const outcome = await app.invoke("scheduler_run_now", { id: saved.id });
    assert.equal(outcome.id, saved.id);
    assert.ok(outcome.status, "run now should report a status");
    assert.ok(Number.isFinite(outcome.durationMs));

    await app.invokeError("scheduler_run_now", { id: "missing-task-id" });
  });
});
