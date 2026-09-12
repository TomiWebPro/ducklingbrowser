"use client";

import { invoke } from "@tauri-apps/api/core";
import { Eye, EyeOff, Loader2, SendHorizontal } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuBot, LuKey, LuPlus, LuRefreshCw, LuTrash2 } from "react-icons/lu";
import { ChangeCard, type ChangeCardData } from "@/components/change-card";
import {
  AnimatedTabs,
  AnimatedTabsContent,
  AnimatedTabsList,
  AnimatedTabsTrigger,
} from "@/components/ui/animated-tabs";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FadingScrollArea } from "@/components/ui/fading-scroll-area";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  AI_PROVIDERS,
  type AiKeyInfo,
  type AiTab,
  type ProbeResult,
  providerLabel,
  providerMeta,
  useAiKeys,
  useCliAgents,
  validateEndpoint,
} from "@/lib/ai";
import { translateBackendError } from "@/lib/backend-errors";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";
import { cn } from "@/lib/utils";

interface AiDialogProps {
  isOpen: boolean;
  onClose: () => void;
  subPage?: boolean;
  /** Which tab to display first when the dialog mounts; defaults to "chat". */
  initialTab?: AiTab;
}

interface AgentChatResult {
  reply: string;
  cards: ChangeCardData[];
  usage?: { total_tokens: number } | null;
  steps_used?: number;
}

interface ChatMessageView {
  role: "user" | "assistant";
  content: string;
}

function ChatPanel({
  keys,
  onNeedKeys,
}: {
  keys: AiKeyInfo[];
  onNeedKeys: () => void;
}) {
  const { t } = useTranslation();
  const agents = useCliAgents(true);
  const [selectedKey, setSelectedKey] = useState<string>("");
  const [useAgent, setUseAgent] = useState<string>("");
  const [messages, setMessages] = useState<ChatMessageView[]>([]);
  const [cards, setCards] = useState<ChangeCardData[]>([]);
  const [input, setInput] = useState("");
  const [running, setRunning] = useState(false);
  const [fullAuto, setFullAuto] = useState(false);
  const [activeRuns, setActiveRuns] = useState<
    { run_id: string; label: string; step: string; elapsed_ms: number }[]
  >([]);
  const [busyCards, setBusyCards] = useState<Set<string>>(new Set());
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (keys.length > 0 && !keys.some((k) => k.id === selectedKey)) {
      setSelectedKey(keys[0].id);
    }
  }, [keys, selectedKey]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
  });

  useEffect(() => {
    let cancelled = false;
    const poll = () => {
      void invoke<
        { run_id: string; label: string; step: string; elapsed_ms: number }[]
      >("agent_active_runs")
        .then((runs) => {
          if (!cancelled) setActiveRuns(runs);
        })
        .catch(() => {});
    };
    poll();
    const timer = setInterval(poll, 3000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  const send = async () => {
    const message = input.trim();
    if (!message || running) return;
    if (!selectedKey && !useAgent) {
      showErrorToast(t("agentChat.noKeySelected"));
      return;
    }
    setMessages((prev) => [...prev, { role: "user", content: message }]);
    setInput("");
    setRunning(true);
    setCards([]);
    try {
      const result = await invoke<AgentChatResult>("agent_chat", {
        keyId: useAgent ? null : selectedKey || null,
        model: null,
        message,
        useAgent: useAgent || null,
        autoApprove: fullAuto,
      });
      setMessages((prev) => [
        ...prev,
        { role: "assistant", content: result.reply },
      ]);
      setCards(result.cards);
    } catch (e) {
      const error = translateBackendError(t, e);
      setMessages((prev) => [...prev, { role: "assistant", content: error }]);
    } finally {
      setRunning(false);
    }
  };

  const confirmCard = async (id: string) => {
    setBusyCards((prev) => new Set(prev).add(id));
    try {
      const result = await invoke<{
        applied: Array<{ id: string }>;
        errors: Array<{ id: string; error: string }>;
      }>("agent_chat_confirm", { cardIds: [id] });
      const applied = result.applied.some((a) => a.id === id);
      if (applied) {
        showSuccessToast(t("changeCard.applied"));
        setCards((prev) => prev.filter((c) => c.id !== id));
      } else {
        const err = result.errors.find((e) => e.id === id);
        showErrorToast(err?.error ?? t("changeCard.applyFailed"));
      }
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setBusyCards((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    }
  };

  const declineCard = async (id: string) => {
    try {
      await invoke("agent_chat_decline", { cardIds: [id] });
      showSuccessToast(t("changeCard.declined"));
      setCards((prev) => prev.filter((c) => c.id !== id));
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  };

  const stopAllRuns = async () => {
    try {
      await Promise.all(
        activeRuns.map((run) =>
          invoke("agent_cancel_run", { runId: run.run_id }).catch(() => {}),
        ),
      );
      showSuccessToast(t("agentChat.stopAllDone"));
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  };

  if (keys.length === 0 && !useAgent && agents.length === 0) {
    return (
      <div className="flex flex-col items-center gap-3 py-12 text-center">
        <LuKey className="size-8 text-muted-foreground" />
        <p className="text-sm text-muted-foreground">{t("agentChat.empty")}</p>
        <Button onClick={onNeedKeys}>{t("agentChat.goToKeys")}</Button>
      </div>
    );
  }

  return (
    <>
      <div className="flex shrink-0 items-center gap-2">
        <Select
          value={useAgent ? `agent:${useAgent}` : selectedKey}
          onValueChange={(v) => {
            if (v.startsWith("agent:")) {
              setUseAgent(v.slice(6));
            } else {
              setUseAgent("");
              setSelectedKey(v);
            }
          }}
        >
          <SelectTrigger className="w-52">
            <SelectValue placeholder={t("agentChat.pickModel")} />
          </SelectTrigger>
          <SelectContent>
            {agents.map((a) => (
              <SelectItem key={a.id} value={`agent:${a.id}`}>
                {a.display_name}
              </SelectItem>
            ))}
            {agents.length > 0 && (
              <SelectItem value="__divider__" disabled>
                {t("agentChat.orDirect")}
              </SelectItem>
            )}
            {keys.map((k) => (
              <SelectItem key={k.id} value={k.id}>
                {k.name} ({k.provider})
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <div
          className="flex items-center gap-1.5"
          title={t("agentChat.fullAutomationHint")}
        >
          <Checkbox
            id="agent-full-auto"
            checked={fullAuto}
            disabled={running || Boolean(useAgent)}
            onCheckedChange={(checked) => setFullAuto(Boolean(checked))}
          />
          <Label
            htmlFor="agent-full-auto"
            className="cursor-pointer text-xs font-medium whitespace-nowrap"
          >
            {t("agentChat.fullAutomation")}
          </Label>
        </div>
        {activeRuns.length > 0 && (
          <button
            type="button"
            onClick={() => void stopAllRuns()}
            title={activeRuns.map((r) => `${r.label}: ${r.step}`).join("\n")}
            className="flex shrink-0 items-center gap-1.5 rounded-full bg-warning/10 px-2 py-1 text-xs text-muted-foreground hover:text-foreground"
          >
            <span className="size-1.5 animate-pulse rounded-full bg-warning" />
            {t("agentChat.activeRuns", { count: activeRuns.length })}
          </button>
        )}
      </div>

      {fullAuto && !useAgent && (
        <p className="shrink-0 rounded-md bg-warning/10 px-2 py-1 text-xs text-muted-foreground">
          {t("agentChat.fullAutomationActive")}
        </p>
      )}

      <div
        ref={scrollRef}
        className="min-h-0 flex-1 space-y-3 overflow-y-auto px-1 py-1"
      >
        {messages.length === 0 && (
          <p className="text-center text-sm text-muted-foreground">
            {t("agentChat.welcome")}
          </p>
        )}
        {messages.map((m, i) => (
          <div key={i} className={m.role === "user" ? "flex justify-end" : ""}>
            <div
              className={
                m.role === "user"
                  ? "max-w-[85%] rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground"
                  : "max-w-[95%] rounded-lg bg-muted px-3 py-2 text-sm"
              }
            >
              {m.content}
            </div>
          </div>
        ))}
        {running && (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" />
            {t("agentChat.working")}
          </div>
        )}
      </div>

      {cards.length > 0 && (
        <div className="space-y-2 border-t pt-3">
          <p className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
            {t("changeCard.sectionTitle")}
          </p>
          <FadingScrollArea className="max-h-64">
            <div className="space-y-2 pr-1">
              {cards.map((card) => (
                <ChangeCard
                  key={card.id}
                  card={card}
                  busy={busyCards.has(card.id)}
                  onConfirm={confirmCard}
                  onDecline={declineCard}
                />
              ))}
            </div>
          </FadingScrollArea>
        </div>
      )}

      <div className="flex shrink-0 items-center gap-2 border-t pt-3">
        <Input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void send();
            }
          }}
          placeholder={t("agentChat.placeholder")}
          disabled={running}
        />
        <Button
          size="icon"
          onClick={() => void send()}
          disabled={running || !input.trim()}
          aria-label={t("agentChat.send")}
        >
          <SendHorizontal className="size-4" />
        </Button>
      </div>
    </>
  );
}

function EndpointsPanel({
  keys,
  loadKeys,
}: {
  keys: AiKeyInfo[];
  loadKeys: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const [provider, setProvider] = useState<string>("openai");
  const [name, setName] = useState("");
  const [model, setModel] = useState(
    providerMeta("openai")?.defaultModel ?? "gpt-4o-mini",
  );
  const [keyValue, setKeyValue] = useState("");
  const [endpoint, setEndpoint] = useState(
    providerMeta("openai")?.defaultEndpoint ?? "",
  );
  const [endpointTouched, setEndpointTouched] = useState(false);
  const [showKey, setShowKey] = useState(false);
  const [saving, setSaving] = useState(false);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [modelOptions, setModelOptions] = useState<string[]>(
    providerMeta("openai")?.models ?? [],
  );
  const [modelsLoading, setModelsLoading] = useState(false);
  const fetchSeq = useRef(0);

  const meta = providerMeta(provider);
  const isOpencodeGo = provider === "opencode";
  const endpointError = (() => {
    if (!meta?.showEndpoint) return null;
    if (!endpoint.trim()) {
      return meta.requireEndpoint && (endpointTouched || !keyValue)
        ? "required"
        : null;
    }
    return validateEndpoint(endpoint);
  })();

  /** Query the endpoint's live model catalog; fall back to the static
   * shortcuts when unreachable. Live ids come first, static extras appended. */
  const refreshModels = useCallback(
    async (nextProvider: string, nextEndpoint: string, nextKey: string) => {
      const seq = ++fetchSeq.current;
      setModelsLoading(true);
      try {
        const live = await invoke<string[]>("ai_keys_models", {
          provider: nextProvider,
          key: nextKey.trim() ? nextKey.trim() : null,
          endpoint: nextEndpoint.trim() ? nextEndpoint.trim() : null,
        });
        if (seq !== fetchSeq.current) return;
        const statik = providerMeta(nextProvider)?.models ?? [];
        const merged = [...live, ...statik.filter((m) => !live.includes(m))];
        if (merged.length > 0) setModelOptions(merged);
      } catch {
        // Unreachable endpoint or bad input: keep the static shortcuts.
      } finally {
        if (seq === fetchSeq.current) setModelsLoading(false);
      }
    },
    [],
  );

  const handleProviderChange = (value: string) => {
    setProvider(value);
    const next = providerMeta(value);
    if (next) {
      if (next.defaultModel) setModel(next.defaultModel);
      // The whole point of picking a provider: the box always shows that
      // provider's URL (editable). Custom has none, so it clears for input.
      setEndpoint(next.defaultEndpoint ?? "");
      setEndpointTouched(false);
      // Options follow the provider immediately; the live catalog refines them.
      setModelOptions(next.models);
      void refreshModels(value, next.defaultEndpoint ?? "", keyValue);
    }
  };

  const handleSave = async (testAfter: boolean) => {
    const effectiveName = isOpencodeGo && !name.trim() ? "OpenCode Go" : name;
    if (
      (!isOpencodeGo && !effectiveName.trim()) ||
      !model.trim() ||
      !keyValue.trim()
    ) {
      showErrorToast(t("aiKeys.emptyFields"));
      return;
    }
    if (meta?.showEndpoint && endpoint.trim() && validateEndpoint(endpoint)) {
      setEndpointTouched(true);
      showErrorToast(t("aiKeys.endpointInvalid"));
      return;
    }
    if (meta?.requireEndpoint && !endpoint.trim()) {
      setEndpointTouched(true);
      showErrorToast(t("aiKeys.endpointRequired"));
      return;
    }
    setSaving(true);
    try {
      await invoke<AiKeyInfo>("ai_keys_save", {
        provider,
        name: effectiveName,
        model,
        key: keyValue,
        endpoint:
          meta?.showEndpoint && endpoint.trim() ? endpoint.trim() : null,
      });
      if (testAfter) {
        const result = await invoke<ProbeResult>("ai_keys_test", {
          provider,
          model,
          key: keyValue,
          endpoint:
            meta?.showEndpoint && endpoint.trim() ? endpoint.trim() : null,
        });
        if (result.ok) {
          showSuccessToast(t("aiKeys.testSuccess"));
        } else {
          showErrorToast(t("aiKeys.testFailed", { detail: result.detail }));
        }
      }
      showSuccessToast(t("aiKeys.saved"));
      setKeyValue("");
      setName("");
      setShowKey(false);
      setEndpointTouched(false);
      await loadKeys();
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (id: string) => {
    setBusyIds((prev) => new Set(prev).add(id));
    try {
      await invoke("ai_keys_delete", { id });
      showSuccessToast(t("aiKeys.deleted"));
      await loadKeys();
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setBusyIds((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    }
  };

  const handleTest = async (keyInfo: AiKeyInfo) => {
    setBusyIds((prev) => new Set(prev).add(keyInfo.id));
    try {
      const result = await invoke<ProbeResult>("ai_keys_test", {
        provider: keyInfo.provider,
        model: keyInfo.model,
        id: keyInfo.id,
      });
      if (result.ok) {
        showSuccessToast(t("aiKeys.testSuccess"));
      } else {
        showErrorToast(t("aiKeys.testFailed", { detail: result.detail }));
      }
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setBusyIds((prev) => {
        const next = new Set(prev);
        next.delete(keyInfo.id);
        return next;
      });
    }
  };

  return (
    <div className="space-y-6 px-1 py-1">
      <section className="space-y-3">
        <h3 className="text-sm font-medium">{t("aiKeys.addTitle")}</h3>
        <div className="grid grid-cols-2 gap-3">
          <div className="space-y-1.5">
            <Label>{t("aiKeys.provider")}</Label>
            <Select value={provider} onValueChange={handleProviderChange}>
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {AI_PROVIDERS.map((p) => (
                  <SelectItem key={p.id} value={p.id}>
                    {providerLabel(t, p.id)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-1.5">
            <Label className="flex items-center gap-1.5">
              {t("aiKeys.model")}
              {modelsLoading && (
                <Loader2
                  className="size-3 animate-spin text-muted-foreground"
                  aria-hidden="true"
                />
              )}
            </Label>
            <Input
              value={model}
              onChange={(e) => setModel(e.target.value)}
              placeholder={meta?.defaultModel || "gpt-4o-mini"}
              list={modelOptions.length ? "ai-model-suggestions" : undefined}
            />
            {modelOptions.length ? (
              <datalist id="ai-model-suggestions">
                {modelOptions.map((m) => (
                  <option key={m} value={m} />
                ))}
              </datalist>
            ) : null}
          </div>
        </div>
        {meta?.showEndpoint && (
          <div className="space-y-1.5">
            <Label>{t("aiKeys.endpoint")}</Label>
            <Input
              value={endpoint}
              onChange={(e) => {
                setEndpoint(e.target.value);
                setEndpointTouched(true);
              }}
              onBlur={() => {
                if (!validateEndpoint(endpoint)) {
                  void refreshModels(provider, endpoint, keyValue);
                }
              }}
              placeholder={
                meta.endpointPlaceholder || t("aiKeys.endpointPlaceholder")
              }
              className={cn(endpointError && "border-destructive")}
              aria-invalid={Boolean(endpointError)}
            />
            {endpointError ? (
              <p className="text-xs text-destructive">
                {endpointError === "required"
                  ? t("aiKeys.endpointRequired")
                  : t("aiKeys.endpointInvalid")}
              </p>
            ) : (
              <p className="text-xs text-muted-foreground">
                {t("aiKeys.endpointHint")}
              </p>
            )}
          </div>
        )}
        {!isOpencodeGo && (
          <div className="space-y-1.5">
            <Label>{t("aiKeys.name")}</Label>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t("aiKeys.namePlaceholder")}
            />
          </div>
        )}
        <div className="space-y-1.5">
          <Label>{t("aiKeys.key")}</Label>
          <div className="relative">
            <Input
              type={showKey ? "text" : "password"}
              value={keyValue}
              onChange={(e) => setKeyValue(e.target.value)}
              onBlur={() => {
                if (keyValue.trim()) {
                  void refreshModels(provider, endpoint, keyValue);
                }
              }}
              placeholder="sk-..."
              className="pr-9"
            />
            <button
              type="button"
              onClick={() => setShowKey((s) => !s)}
              className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
              aria-label={showKey ? t("aiKeys.hideKey") : t("aiKeys.showKey")}
            >
              {showKey ? (
                <EyeOff className="size-4" />
              ) : (
                <Eye className="size-4" />
              )}
            </button>
          </div>
        </div>
        <div className="flex gap-2 pt-1">
          <Button
            onClick={() => void handleSave(false)}
            disabled={saving}
            className="flex-1"
          >
            {saving ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <LuPlus className="size-4" />
            )}
            {t("aiKeys.save")}
          </Button>
          <Button
            variant="secondary"
            onClick={() => void handleSave(true)}
            disabled={saving}
            className="flex-1"
          >
            {saving ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <LuRefreshCw className="size-4" />
            )}
            {t("aiKeys.saveAndTest")}
          </Button>
        </div>
      </section>

      <section className="space-y-3">
        <h3 className="text-sm font-medium">{t("aiKeys.storedTitle")}</h3>
        {keys.length === 0 ? (
          <p className="text-sm text-muted-foreground">{t("aiKeys.empty")}</p>
        ) : (
          <div className="space-y-2">
            {keys.map((k) => (
              <div
                key={k.id}
                className="flex items-center justify-between gap-3 rounded-lg border p-3"
              >
                <div className="min-w-0 space-y-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate text-sm font-medium">
                      {k.name}
                    </span>
                    <Badge variant="secondary" className="text-xs">
                      {providerLabel(t, k.provider)}
                    </Badge>
                  </div>
                  <p className="truncate text-xs text-muted-foreground">
                    {k.model} · {k.masked_key}
                    {k.endpoint ? ` · ${k.endpoint}` : ""}
                  </p>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <Button
                    variant="ghost"
                    size="icon"
                    disabled={busyIds.has(k.id)}
                    onClick={() => void handleTest(k)}
                    title={t("aiKeys.test")}
                    aria-label={t("aiKeys.test")}
                  >
                    {busyIds.has(k.id) ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <LuRefreshCw className="size-4" />
                    )}
                  </Button>
                  <Button
                    variant="ghost"
                    size="icon"
                    disabled={busyIds.has(k.id)}
                    onClick={() => void handleDelete(k.id)}
                    title={t("aiKeys.delete")}
                    aria-label={t("aiKeys.delete")}
                    className={cn(
                      "text-muted-foreground hover:text-destructive",
                    )}
                  >
                    <LuTrash2 className="size-4" />
                  </Button>
                </div>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}

export function AiDialog({
  isOpen,
  onClose,
  subPage,
  initialTab = "chat",
}: AiDialogProps) {
  const { t } = useTranslation();
  const { keys, loadKeys } = useAiKeys(t, isOpen);
  const [activeTab, setActiveTab] = useState<AiTab>(initialTab);

  const goToEndpoints = useCallback(() => {
    setActiveTab("endpoints");
  }, []);

  return (
    <Dialog open={isOpen} onOpenChange={onClose} subPage={subPage}>
      <DialogContent className="max-w-2xl flex flex-col">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <LuBot className="size-4 text-muted-foreground" />
            {t("ai.title")}
          </DialogTitle>
        </DialogHeader>

        <AnimatedTabs
          key={initialTab}
          defaultValue={initialTab}
          value={activeTab}
          onValueChange={(v) => setActiveTab(v as AiTab)}
          className="flex min-h-0 flex-1 flex-col"
        >
          <AnimatedTabsList
            className={cn(
              "w-full",
              subPage &&
                "!bg-transparent !p-0 !h-auto !rounded-none justify-start gap-4",
            )}
          >
            <AnimatedTabsTrigger
              value="chat"
              className={cn(
                "flex-1",
                subPage &&
                  "!flex-none !rounded-none !bg-transparent !shadow-none data-[state=active]:!bg-transparent data-[state=active]:!text-foreground data-[state=active]:!shadow-none text-muted-foreground hover:text-foreground !px-1 !py-1 text-xs",
              )}
            >
              <LuBot className="size-3.5" />
              {t("ai.tabs.chat")}
            </AnimatedTabsTrigger>
            <AnimatedTabsTrigger
              value="endpoints"
              className={cn(
                "flex-1",
                subPage &&
                  "!flex-none !rounded-none !bg-transparent !shadow-none data-[state=active]:!bg-transparent data-[state=active]:!text-foreground data-[state=active]:!shadow-none text-muted-foreground hover:text-foreground !px-1 !py-1 text-xs",
              )}
            >
              <LuKey className="size-3.5" />
              {t("ai.tabs.endpoints")}
            </AnimatedTabsTrigger>
          </AnimatedTabsList>

          <AnimatedTabsContent
            value="chat"
            className="mt-4 min-h-0 flex-1 flex-col gap-3 data-[state=active]:flex"
          >
            <ChatPanel keys={keys} onNeedKeys={goToEndpoints} />
          </AnimatedTabsContent>

          <AnimatedTabsContent value="endpoints" className="mt-4">
            <FadingScrollArea className="max-h-[70vh] flex-1">
              <EndpointsPanel keys={keys} loadKeys={loadKeys} />
            </FadingScrollArea>
          </AnimatedTabsContent>
        </AnimatedTabs>
      </DialogContent>
    </Dialog>
  );
}
