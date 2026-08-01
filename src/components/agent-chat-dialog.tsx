"use client";

import { invoke } from "@tauri-apps/api/core";
import { Loader2, SendHorizontal } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuBot, LuKey } from "react-icons/lu";
import { ChangeCard, type ChangeCardData } from "@/components/change-card";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FadingScrollArea } from "@/components/ui/fading-scroll-area";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { translateBackendError } from "@/lib/backend-errors";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";

interface AiKeyInfo {
  id: string;
  provider: string;
  name: string;
  model: string;
  masked_key: string;
  created_at: string;
}

interface McpAgentInfo {
  id: string;
  display_name: string;
  category: string;
  connected: boolean;
  detected: boolean;
}

interface ChatMessageView {
  role: "user" | "assistant";
  content: string;
}

interface AgentChatResult {
  reply: string;
  cards: ChangeCardData[];
}

interface AgentChatDialogProps {
  isOpen: boolean;
  onClose: () => void;
  subPage?: boolean;
  onGoToKeys: () => void;
}

export function AgentChatDialog({
  isOpen,
  onClose,
  subPage,
  onGoToKeys,
}: AgentChatDialogProps) {
  const { t } = useTranslation();
  const [keys, setKeys] = useState<AiKeyInfo[]>([]);
  const [agents, setAgents] = useState<McpAgentInfo[]>([]);
  const [selectedKey, setSelectedKey] = useState<string>("");
  const [useAgent, setUseAgent] = useState<string>("");
  const [messages, setMessages] = useState<ChatMessageView[]>([]);
  const [cards, setCards] = useState<ChangeCardData[]>([]);
  const [input, setInput] = useState("");
  const [running, setRunning] = useState(false);
  const [busyCards, setBusyCards] = useState<Set<string>>(new Set());
  const scrollRef = useRef<HTMLDivElement>(null);

  const loadKeys = useCallback(async () => {
    try {
      const loaded = await invoke<AiKeyInfo[]>("ai_keys_list");
      setKeys(loaded);
      if (loaded.length > 0 && !loaded.some((k) => k.id === selectedKey)) {
        setSelectedKey(loaded[0].id);
      }
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  }, [t, selectedKey]);

  const loadAgents = useCallback(async () => {
    try {
      const loaded = await invoke<McpAgentInfo[]>("list_mcp_agents");
      setAgents(loaded.filter((a) => a.category === "cli" && a.detected));
    } catch {
      // Agent list is optional in the header picker.
    }
  }, []);

  useEffect(() => {
    if (isOpen) {
      void loadKeys();
      void loadAgents();
    }
  }, [isOpen, loadKeys, loadAgents]);

  useEffect(() => {
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
  });

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

  return (
    <Dialog open={isOpen} onOpenChange={onClose} subPage={subPage}>
      <DialogContent className="max-w-2xl flex flex-col">
        <DialogHeader className="flex flex-row items-center justify-between gap-3">
          <DialogTitle className="flex items-center gap-2">
            <LuBot className="size-4 text-muted-foreground" />
            {t("agentChat.title")}
          </DialogTitle>
          <div className="flex items-center gap-2">
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
          </div>
        </DialogHeader>

        {keys.length === 0 && !useAgent ? (
          <div className="flex flex-col items-center gap-3 py-12 text-center">
            <LuKey className="size-8 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">
              {t("agentChat.empty")}
            </p>
            <Button onClick={onGoToKeys}>{t("agentChat.goToKeys")}</Button>
          </div>
        ) : (
          <>
            <div
              ref={scrollRef}
              className="max-h-[55vh] flex-1 space-y-3 overflow-y-auto px-1 py-1"
            >
              {messages.length === 0 && (
                <p className="text-center text-sm text-muted-foreground">
                  {t("agentChat.welcome")}
                </p>
              )}
              {messages.map((m, i) => (
                <div
                  key={i}
                  className={m.role === "user" ? "flex justify-end" : ""}
                >
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

            <div className="flex items-center gap-2 border-t pt-3">
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
        )}
      </DialogContent>
    </Dialog>
  );
}
