"use client";

import { invoke } from "@tauri-apps/api/core";
import { Eye, EyeOff, Loader2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuKey, LuPlus, LuRefreshCw, LuTrash2 } from "react-icons/lu";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
import { translateBackendError } from "@/lib/backend-errors";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";
import { cn } from "@/lib/utils";

interface AiKeyInfo {
  id: string;
  provider: string;
  name: string;
  model: string;
  masked_key: string;
  created_at: string;
}

interface ProbeResult {
  ok: boolean;
  detail: string;
}

interface AiKeysDialogProps {
  isOpen: boolean;
  onClose: () => void;
  subPage?: boolean;
}

const PROVIDERS = [
  "anthropic",
  "openai",
  "groq",
  "google",
  "openrouter",
] as const;

const DEFAULT_MODELS: Record<string, string> = {
  anthropic: "claude-sonnet-4-5",
  openai: "gpt-4o-mini",
  groq: "llama-3.3-70b-versatile",
  google: "gemini-2.5-flash",
  openrouter: "anthropic/claude-sonnet-4-5",
};

function providerLabel(t: (k: string) => string, provider: string): string {
  switch (provider) {
    case "anthropic":
      return t("aiKeys.providers.anthropic");
    case "openai":
      return t("aiKeys.providers.openai");
    case "groq":
      return t("aiKeys.providers.groq");
    case "google":
      return t("aiKeys.providers.google");
    case "openrouter":
      return t("aiKeys.providers.openrouter");
    default:
      return provider;
  }
}

export function AiKeysDialog({ isOpen, onClose, subPage }: AiKeysDialogProps) {
  const { t } = useTranslation();
  const [keys, setKeys] = useState<AiKeyInfo[]>([]);
  const [provider, setProvider] = useState<string>("openai");
  const [name, setName] = useState("");
  const [model, setModel] = useState(DEFAULT_MODELS.openai);
  const [keyValue, setKeyValue] = useState("");
  const [showKey, setShowKey] = useState(false);
  const [saving, setSaving] = useState(false);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());

  const loadKeys = useCallback(async () => {
    try {
      setKeys(await invoke<AiKeyInfo[]>("ai_keys_list"));
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  }, [t]);

  useEffect(() => {
    if (isOpen) {
      void loadKeys();
      setKeyValue("");
      setShowKey(false);
    }
  }, [isOpen, loadKeys]);

  const handleProviderChange = (value: string) => {
    setProvider(value);
    setModel((prev) => DEFAULT_MODELS[value] ?? prev);
  };

  const handleSave = async (testAfter: boolean) => {
    if (!name.trim() || !model.trim() || !keyValue.trim()) {
      showErrorToast(t("aiKeys.emptyFields"));
      return;
    }
    setSaving(true);
    try {
      await invoke<AiKeyInfo>("ai_keys_save", {
        provider,
        name,
        model,
        key: keyValue,
      });
      if (testAfter) {
        const result = await invoke<ProbeResult>("ai_keys_test", {
          provider,
          model,
          key: keyValue,
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
    <Dialog open={isOpen} onOpenChange={onClose} subPage={subPage}>
      <DialogContent className="max-w-2xl flex flex-col">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <LuKey className="size-4 text-muted-foreground" />
            {t("aiKeys.title")}
          </DialogTitle>
        </DialogHeader>

        <FadingScrollArea className="max-h-[70vh] flex-1">
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
                      {PROVIDERS.map((p) => (
                        <SelectItem key={p} value={p}>
                          {providerLabel(t, p)}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-1.5">
                  <Label>{t("aiKeys.model")}</Label>
                  <Input
                    value={model}
                    onChange={(e) => setModel(e.target.value)}
                    placeholder="gpt-4o-mini"
                  />
                </div>
              </div>
              <div className="space-y-1.5">
                <Label>{t("aiKeys.name")}</Label>
                <Input
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder={t("aiKeys.namePlaceholder")}
                />
              </div>
              <div className="space-y-1.5">
                <Label>{t("aiKeys.key")}</Label>
                <div className="relative">
                  <Input
                    type={showKey ? "text" : "password"}
                    value={keyValue}
                    onChange={(e) => setKeyValue(e.target.value)}
                    placeholder="sk-..."
                    className="pr-9"
                  />
                  <button
                    type="button"
                    onClick={() => setShowKey((s) => !s)}
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
                    aria-label={
                      showKey ? t("aiKeys.hideKey") : t("aiKeys.showKey")
                    }
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
                <p className="text-sm text-muted-foreground">
                  {t("aiKeys.empty")}
                </p>
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
        </FadingScrollArea>
      </DialogContent>
    </Dialog>
  );
}
