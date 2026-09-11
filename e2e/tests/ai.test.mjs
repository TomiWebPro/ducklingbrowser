import assert from "node:assert/strict";
import test from "node:test";
import { withApp } from "../lib/app.mjs";

test("AI key store CRUD through Tauri commands", async () => {
  await withApp("ai-keys-crud", async (app) => {
    const initial = await app.invoke("ai_keys_list");
    assert.ok(Array.isArray(initial));

    const saved = await app.invoke("ai_keys_save", {
      provider: "openai",
      name: "Primary",
      model: "gpt-4o-mini",
      key: "sk-test-123456",
    });
    assert.ok(saved.id, "save should assign an id");
    assert.equal(saved.provider, "openai");
    assert.equal(saved.masked_key, "sk-***3456");
    assert.ok(!saved.key, "plaintext key must never be returned");

    const listed = await app.invoke("ai_keys_list");
    assert.ok(listed.some((k) => k.id === saved.id));

    const updated = await app.invoke("ai_keys_save", {
      provider: "openai",
      name: "Primary",
      model: "gpt-4o",
      key: "sk-test-abcdef",
    });
    assert.equal(updated.id, saved.id, "same name should overwrite");
    assert.equal(updated.model, "gpt-4o");

    await app.invoke("ai_keys_delete", { id: saved.id });
    const after = await app.invoke("ai_keys_list");
    assert.ok(!after.some((k) => k.id === saved.id));
  });
});

test("AI key store supports custom endpoints and opencode provider", async () => {
  await withApp("ai-keys-endpoints", async (app) => {
    // Custom without an endpoint must fail.
    await app.invokeError("ai_keys_save", {
      provider: "custom",
      name: "Local",
      model: "qwen3",
      key: "ollama",
    });
    // Bad endpoint URLs must fail.
    await app.invokeError("ai_keys_save", {
      provider: "custom",
      name: "Local",
      model: "qwen3",
      key: "ollama",
      endpoint: "ftp://example.com/v1",
    });

    const custom = await app.invoke("ai_keys_save", {
      provider: "custom",
      name: "Local",
      model: "qwen3",
      key: "ollama",
      endpoint: "http://localhost:11434/v1/",
    });
    assert.equal(
      custom.endpoint,
      "http://localhost:11434/v1",
      "endpoint is normalized",
    );

    const oc = await app.invoke("ai_keys_save", {
      provider: "opencode",
      name: "Code",
      model: "opencode",
      key: "local",
    });
    assert.equal(oc.provider, "opencode");
    assert.ok(
      !oc.endpoint,
      "opencode falls back to its Go subscription default",
    );

    // OpenCode Go needs only a key: an empty name defaults to "OpenCode Go".
    const go = await app.invoke("ai_keys_save", {
      provider: "opencode",
      name: "",
      model: "kimi-k3",
      key: "local-go-key",
    });
    assert.equal(go.name, "OpenCode Go");

    // Probing a saved id resolves the stored endpoint without erroring.
    const probed = await app.invoke("ai_keys_test", {
      provider: "custom",
      model: "qwen3",
      id: custom.id,
    });
    assert.equal(typeof probed.ok, "boolean");
    assert.ok(probed.detail.length > 0);

    // Model catalog: invalid input rejects; unreachable endpoints fall back
    // to an empty list (frontend keeps its static suggestions).
    await app.invokeError("ai_keys_models", { provider: "unknown" });
    await app.invokeError("ai_keys_models", {
      provider: "openai",
      id: "missing-key-id",
    });
    await app.invokeError("ai_keys_models", {
      provider: "custom",
      key: "ollama",
      endpoint: "ftp://example.com/v1",
    });
    const models = await app.invoke("ai_keys_models", {
      provider: "opencode",
    });
    assert.ok(Array.isArray(models));
    assert.ok(models.every((m) => typeof m === "string"));

    await app.invoke("ai_keys_delete", { id: custom.id });
    await app.invoke("ai_keys_delete", { id: oc.id });
    await app.invoke("ai_keys_delete", { id: go.id });
  });
});

test("AI key store validates input and probes reject bogus keys", async () => {
  await withApp("ai-keys-validation", async (app) => {
    await app.invokeError("ai_keys_save", {
      provider: "unknown",
      name: "Bad",
      model: "m",
      key: "k",
    });
    await app.invokeError("ai_keys_save", {
      provider: "openai",
      name: "",
      model: "gpt-4o-mini",
      key: "sk-test-123456",
    });
    await app.invokeError("ai_keys_delete", { id: "missing-key-id" });

    const result = await app.invoke("ai_keys_test", {
      provider: "openai",
      model: "gpt-4o-mini",
      key: "sk-definitely-not-a-real-key",
    });
    assert.equal(result.ok, false);
    assert.ok(result.detail.length > 0);
  });
});

test("agent chat requires a configured key and handles cards", async () => {
  await withApp("ai-agent", async (app) => {
    await app.invokeError("agent_chat", {
      keyId: null,
      model: null,
      message: "hello",
      useAgent: null,
    });

    await app.invokeError("agent_chat", {
      keyId: "missing-key",
      model: null,
      message: "hello",
      useAgent: null,
    });

    await app.invokeError("agent_chat", {
      keyId: null,
      model: null,
      message: "hello",
      useAgent: "not-a-real-cli",
    });

    const declined = await app.invoke("agent_chat_decline", {
      cardIds: ["missing-card"],
    });
    assert.deepEqual(declined.declined, ["missing-card"]);

    const confirmed = await app.invoke("agent_chat_confirm", {
      cardIds: ["missing-card"],
    });
    assert.deepEqual(confirmed.applied, []);
    assert.ok(confirmed.errors.length === 1);
  });
});
