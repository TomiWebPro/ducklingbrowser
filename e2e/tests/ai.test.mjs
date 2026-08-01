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
