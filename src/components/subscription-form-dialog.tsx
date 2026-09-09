"use client";

import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LoadingButton } from "@/components/loading-button";
import { AnimatedSwitch } from "@/components/ui/animated-switch";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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
import type { StoredProxy } from "@/types";
import { RippleButton } from "./ui/ripple";

export interface SubscriptionInfo {
  id: string;
  name: string;
  url: string;
  refreshHours: number;
  useProxyId?: string | null;
  autoCheck: boolean;
  autoPrune: boolean;
  lastFetchedAt?: number | null;
  lastStatus?: string | null;
}

interface PreviewResult {
  vpn: number;
  proxy: number;
  unsupported: number;
  suggestedIntervalHours?: number | null;
  userInfo?: string | null;
}

interface SubscriptionFormDialogProps {
  isOpen: boolean;
  onClose: () => void;
  editing: SubscriptionInfo | null;
  proxies: StoredProxy[];
  onSaved: () => void;
}

export function SubscriptionFormDialog({
  isOpen,
  onClose,
  editing,
  proxies,
  onSaved,
}: SubscriptionFormDialogProps) {
  const { t } = useTranslation();
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [refreshHours, setRefreshHours] = useState("24");
  const [useProxyId, setUseProxyId] = useState("__direct__");
  const [autoCheck, setAutoCheck] = useState(false);
  const [autoPrune, setAutoPrune] = useState(false);
  const [saving, setSaving] = useState(false);
  const [previewing, setPreviewing] = useState(false);
  const [preview, setPreview] = useState<PreviewResult | null>(null);

  useEffect(() => {
    if (!isOpen) return;
    setName(editing?.name ?? "");
    setUrl(editing?.url ?? "");
    setRefreshHours(String(editing?.refreshHours ?? 24));
    setUseProxyId(editing?.useProxyId ?? "__direct__");
    setAutoCheck(editing?.autoCheck ?? false);
    setAutoPrune(editing?.autoPrune ?? false);
    setPreview(null);
  }, [isOpen, editing]);

  const handlePreview = async () => {
    if (!url.trim()) {
      showErrorToast(t("subscriptions.form.urlRequired"));
      return;
    }
    setPreviewing(true);
    try {
      const result = await invoke<PreviewResult>("subscription_preview", {
        url: url.trim(),
        useProxyId: useProxyId === "__direct__" ? null : useProxyId,
      });
      setPreview(result);
      if (
        !editing &&
        result.suggestedIntervalHours != null &&
        refreshHours === "24"
      ) {
        setRefreshHours(String(result.suggestedIntervalHours));
      }
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setPreviewing(false);
    }
  };

  const handleSave = async (refreshNow: boolean) => {
    if (!name.trim()) {
      showErrorToast(t("subscriptions.form.nameRequired"));
      return;
    }
    if (!url.trim()) {
      showErrorToast(t("subscriptions.form.urlRequired"));
      return;
    }
    const hours = Number.parseInt(refreshHours, 10);
    if (!Number.isFinite(hours) || hours < 0 || hours > 720) {
      showErrorToast(t("subscriptions.form.intervalInvalid"));
      return;
    }
    setSaving(true);
    try {
      const saved = await invoke<SubscriptionInfo>("subscription_save", {
        id: editing?.id ?? null,
        name: name.trim(),
        url: url.trim(),
        refreshHours: hours,
        useProxyId: useProxyId === "__direct__" ? null : useProxyId,
        autoCheck,
        autoPrune,
      });
      showSuccessToast(t("subscriptions.saved"));
      onSaved();
      if (refreshNow) {
        try {
          const result = await invoke<{
            added: number;
            updated: number;
            pruned: number;
            unsupported: number;
            errors: string[];
          }>("subscription_refresh", { id: saved.id });
          if (result.errors.length > 0) {
            showErrorToast(
              t("subscriptions.refreshPartial", {
                added: result.added,
                errors: result.errors.length,
              }),
            );
          } else {
            showSuccessToast(
              t("subscriptions.refreshSuccess", { added: result.added }),
            );
          }
          onSaved();
        } catch (e) {
          showErrorToast(translateBackendError(t, e));
        }
      }
      onClose();
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>
            {editing
              ? t("subscriptions.form.editTitle")
              : t("subscriptions.form.addTitle")}
          </DialogTitle>
          <DialogDescription>
            {t("subscriptions.form.description")}
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-4">
          <div className="space-y-1.5">
            <Label htmlFor="sub-name">{t("subscriptions.form.name")}</Label>
            <Input
              id="sub-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t("subscriptions.form.namePlaceholder")}
            />
          </div>

          <div className="space-y-1.5">
            <Label htmlFor="sub-url">{t("subscriptions.form.url")}</Label>
            <div className="flex gap-2">
              <Input
                id="sub-url"
                value={url}
                onChange={(e) => {
                  setUrl(e.target.value);
                  setPreview(null);
                }}
                placeholder="https://raw.githubusercontent.com/…/Sub1.txt"
                className="font-mono text-xs"
              />
              <LoadingButton
                isLoading={previewing}
                variant="secondary"
                onClick={() => void handlePreview()}
                disabled={!url.trim()}
              >
                {t("subscriptions.form.preview")}
              </LoadingButton>
            </div>
            {preview && (
              <p className="text-xs text-muted-foreground">
                {t("subscriptions.form.previewResult", {
                  vpn: preview.vpn,
                  proxy: preview.proxy,
                  unsupported: preview.unsupported,
                })}
                {preview.userInfo ? ` · ${preview.userInfo}` : ""}
              </p>
            )}
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1.5">
              <Label htmlFor="sub-interval">
                {t("subscriptions.form.interval")}
              </Label>
              <Input
                id="sub-interval"
                type="number"
                min={0}
                max={720}
                value={refreshHours}
                onChange={(e) => setRefreshHours(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                {t("subscriptions.form.intervalHint")}
              </p>
            </div>
            <div className="space-y-1.5">
              <Label>{t("subscriptions.form.fetchVia")}</Label>
              <Select value={useProxyId} onValueChange={setUseProxyId}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="__direct__">
                    {t("subscriptions.form.direct")}
                  </SelectItem>
                  {proxies.map((p) => (
                    <SelectItem key={p.id} value={p.id}>
                      {p.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>

          <div className="space-y-3">
            <div className="flex items-center justify-between gap-3">
              <div className="space-y-0.5">
                <p className="text-sm font-medium">
                  {t("subscriptions.form.autoCheck")}
                </p>
                <p className="text-xs text-muted-foreground">
                  {t("subscriptions.form.autoCheckHint")}
                </p>
              </div>
              <AnimatedSwitch
                checked={autoCheck}
                onCheckedChange={setAutoCheck}
              />
            </div>
            <div className="flex items-center justify-between gap-3">
              <div className="space-y-0.5">
                <p className="text-sm font-medium">
                  {t("subscriptions.form.autoPrune")}
                </p>
                <p className="text-xs text-muted-foreground">
                  {t("subscriptions.form.autoPruneHint")}
                </p>
              </div>
              <AnimatedSwitch
                checked={autoPrune}
                onCheckedChange={setAutoPrune}
              />
            </div>
          </div>
        </div>

        <DialogFooter>
          <RippleButton variant="outline" onClick={onClose}>
            {t("common.buttons.cancel")}
          </RippleButton>
          <LoadingButton
            isLoading={saving}
            variant="secondary"
            onClick={() => void handleSave(false)}
          >
            {t("subscriptions.form.save")}
          </LoadingButton>
          <LoadingButton
            isLoading={saving}
            onClick={() => void handleSave(true)}
          >
            {t("subscriptions.form.saveAndRefresh")}
          </LoadingButton>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
