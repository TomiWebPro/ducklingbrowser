import { invoke } from "@tauri-apps/api/core";
import type { TFunction } from "i18next";
import { useCallback, useEffect, useState } from "react";
import { translateBackendError } from "@/lib/backend-errors";
import { showErrorToast } from "@/lib/toast-utils";

export interface AiKeyInfo {
  id: string;
  provider: string;
  name: string;
  model: string;
  masked_key: string;
  created_at: string;
  endpoint?: string | null;
}

export interface ProbeResult {
  ok: boolean;
  detail: string;
}

export interface McpAgentInfo {
  id: string;
  display_name: string;
  category: string;
  connected: boolean;
  detected: boolean;
}

export type AiTab = "chat" | "endpoints";

export interface ProviderMeta {
  id: string;
  defaultModel: string;
  defaultEndpoint?: string;
  /** Show the endpoint input for this provider. */
  showEndpoint: boolean;
  /** Endpoint is mandatory (custom). */
  requireEndpoint: boolean;
  endpointPlaceholder: string;
  /**
   * Selectable model suggestions for the combobox. Custom entry is always
   * allowed — these are just shortcuts, so the list stays useful without a
   * live catalog fetch.
   */
  models: string[];
}

export const AI_PROVIDERS: ProviderMeta[] = [
  {
    id: "openai",
    defaultModel: "gpt-4o-mini",
    defaultEndpoint: "https://api.openai.com/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "https://api.openai.com/v1",
    models: ["gpt-4o-mini", "gpt-4o"],
  },
  {
    id: "anthropic",
    defaultModel: "claude-sonnet-5",
    showEndpoint: false,
    requireEndpoint: false,
    endpointPlaceholder: "",
    models: [
      "claude-sonnet-5",
      "claude-opus-5",
      "claude-sonnet-4-6",
      "claude-opus-4-6",
      "claude-haiku-4-5",
      "claude-sonnet-4-5",
    ],
  },
  {
    id: "groq",
    defaultModel: "openai/gpt-oss-120b",
    defaultEndpoint: "https://api.groq.com/openai/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "https://api.groq.com/openai/v1",
    models: [
      "openai/gpt-oss-120b",
      "openai/gpt-oss-20b",
      "qwen/qwen3.6-27b",
      "qwen/qwen3.8-27b",
    ],
  },
  {
    id: "google",
    defaultModel: "gemini-2.5-flash",
    showEndpoint: false,
    requireEndpoint: false,
    endpointPlaceholder: "",
    models: ["gemini-2.5-flash", "gemini-2.5-pro"],
  },
  {
    id: "openrouter",
    defaultModel: "anthropic/claude-sonnet-4-5",
    defaultEndpoint: "https://openrouter.ai/api/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "https://openrouter.ai/api/v1",
    models: [
      "anthropic/claude-sonnet-4-5",
      "anthropic/claude-sonnet-4.6",
      "openai/gpt-4o-mini",
      "openai/gpt-4o",
      "x-ai/grok-4.6",
      "x-ai/grok-4.5",
    ],
  },
  {
    id: "opencode",
    defaultModel: "kimi-k3",
    defaultEndpoint: "https://opencode.ai/zen/go/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "https://opencode.ai/zen/go/v1",
    models: [
      "kimi-k3",
      "grok-4.5",
      "grok-4.6",
      "gpt-5.6-luna",
      "glm-5",
      "glm-5.1",
      "glm-5.2",
      "glm-5.3",
      "glm-5.3-flash",
      "kimi-k2.5",
      "kimi-k2.6",
      "kimi-k2.7-code",
      "longcat-2.0",
      "deepseek-flash",
      "deepseek-v4-flash",
      "deepseek-v4-pro",
      "deepseek-v4-flash-vision-exp",
      "mimo-v2.5",
      "mimo-v2.5-pro",
      "muse-spark-1.3-contributor",
      "muse-spark-1.2-contributor",
      "minimax-m3",
      "minimax-m2.7",
      "minimax-m2.5",
      "qwen3.8-max",
      "qwen3.8-flash",
      "qwen3.7-max",
      "qwen3.7-plus",
      "qwen3.6-plus",
      "qwen3.5-plus",
      "hy3",
      "hy4-preview",
      "omen-alpha",
    ],
  },
  {
    id: "custom",
    defaultModel: "",
    showEndpoint: true,
    requireEndpoint: true,
    endpointPlaceholder: "http://localhost:11434/v1",
    models: [],
  },
];

export function providerMeta(id: string): ProviderMeta | undefined {
  return AI_PROVIDERS.find((p) => p.id === id);
}

export function providerLabel(t: TFunction, provider: string): string {
  const key = `aiKeys.providers.${provider}`;
  const translated = t(key);
  // i18next returns the key itself when missing — fall back to raw id.
  return translated === key ? provider : translated;
}

/** Frontend endpoint validation mirroring the backend `normalize_endpoint`. */
export function validateEndpoint(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) return "empty";
  if (trimmed.length > 2000) return "too-long";
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return "invalid";
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return "scheme";
  }
  if (!parsed.hostname) return "host";
  return null;
}

/** Shared key-vault state: single fetch, single source of truth. */
export function useAiKeys(t: TFunction, isOpen: boolean) {
  const [keys, setKeys] = useState<AiKeyInfo[]>([]);

  const loadKeys = useCallback(async () => {
    try {
      setKeys(await invoke<AiKeyInfo[]>("ai_keys_list"));
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  }, [t]);

  useEffect(() => {
    if (isOpen) void loadKeys();
  }, [isOpen, loadKeys]);

  return { keys, setKeys, loadKeys };
}

/** Installed CLI agents usable as chat delegation targets. */
export function useCliAgents(isOpen: boolean) {
  const [agents, setAgents] = useState<McpAgentInfo[]>([]);

  useEffect(() => {
    if (!isOpen) return;
    invoke<McpAgentInfo[]>("list_mcp_agents")
      .then((loaded) => {
        setAgents(loaded.filter((a) => a.category === "cli" && a.detected));
      })
      .catch(() => {
        // Agent list is optional in the header picker.
      });
  }, [isOpen]);

  return agents;
}
