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
}

export const AI_PROVIDERS: ProviderMeta[] = [
  {
    id: "openai",
    defaultModel: "gpt-4o-mini",
    defaultEndpoint: "https://api.openai.com/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "https://api.openai.com/v1",
  },
  {
    id: "anthropic",
    defaultModel: "claude-sonnet-4-5",
    showEndpoint: false,
    requireEndpoint: false,
    endpointPlaceholder: "",
  },
  {
    id: "groq",
    defaultModel: "llama-3.3-70b-versatile",
    defaultEndpoint: "https://api.groq.com/openai/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "https://api.groq.com/openai/v1",
  },
  {
    id: "google",
    defaultModel: "gemini-2.5-flash",
    showEndpoint: false,
    requireEndpoint: false,
    endpointPlaceholder: "",
  },
  {
    id: "openrouter",
    defaultModel: "anthropic/claude-sonnet-4-5",
    defaultEndpoint: "https://openrouter.ai/api/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "https://openrouter.ai/api/v1",
  },
  {
    id: "opencode",
    defaultModel: "opencode",
    defaultEndpoint: "http://localhost:4096/v1",
    showEndpoint: true,
    requireEndpoint: false,
    endpointPlaceholder: "http://localhost:4096/v1",
  },
  {
    id: "custom",
    defaultModel: "",
    showEndpoint: true,
    requireEndpoint: true,
    endpointPlaceholder: "http://localhost:11434/v1",
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
