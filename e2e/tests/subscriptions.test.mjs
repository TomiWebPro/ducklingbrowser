import assert from "node:assert/strict";
import test from "node:test";
import { withApp } from "../lib/app.mjs";

test("subscription CRUD, validation, and preview failure paths", async () => {
  await withApp("subscriptions-crud", async (app) => {
    const initial = await app.invoke("subscriptions_list");
    assert.ok(Array.isArray(initial));

    await app.invokeError("subscription_save", {
      id: null,
      name: "",
      url: "https://example.com/sub.txt",
      refreshHours: 24,
      useProxyId: null,
      autoCheck: false,
      autoPrune: false,
    });
    await app.invokeError("subscription_save", {
      id: null,
      name: "Bad",
      url: "ftp://example.com/sub.txt",
      refreshHours: 24,
      useProxyId: null,
      autoCheck: false,
      autoPrune: false,
    });
    await app.invokeError("subscription_save", {
      id: null,
      name: "Bad",
      url: "https://example.com/sub.txt",
      refreshHours: 9999,
      useProxyId: null,
      autoCheck: false,
      autoPrune: false,
    });
    await app.invokeError("subscription_delete", {
      id: "missing-sub-id",
      deleteEntries: false,
    });

    const saved = await app.invoke("subscription_save", {
      id: null,
      name: "Pool",
      url: "https://example.com/sub.txt",
      refreshHours: 24,
      useProxyId: null,
      autoCheck: false,
      autoPrune: true,
    });
    assert.ok(saved.id, "save should assign an id");
    assert.equal(saved.refreshHours, 24);
    assert.equal(saved.autoPrune, true);

    const listed = await app.invoke("subscriptions_list");
    assert.ok(listed.some((s) => s.id === saved.id));

    const entries = await app.invoke("subscription_entries", {
      subscriptionId: saved.id,
    });
    assert.deepEqual(entries, []);

    // Invalid URLs fail before any network access.
    await app.invokeError("subscription_preview", {
      url: "not-a-url",
      useProxyId: null,
    });

    // Refresh against an unreachable URL fails fast with connection refused.
    const unreachable = await app.invoke("subscription_save", {
      id: null,
      name: "Unreachable",
      url: "http://127.0.0.1:9/sub.txt",
      refreshHours: 0,
      useProxyId: null,
      autoCheck: false,
      autoPrune: false,
    });
    await app.invokeError("subscription_refresh", { id: unreachable.id });
    await app.invoke("subscription_delete", {
      id: unreachable.id,
      deleteEntries: true,
    });

    await app.invoke("subscription_delete", {
      id: saved.id,
      deleteEntries: true,
    });
    const after = await app.invoke("subscriptions_list");
    assert.ok(!after.some((s) => s.id === saved.id));
  });
});
