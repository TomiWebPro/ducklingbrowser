"use client";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Loader2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuPencil, LuRefreshCw, LuTrash2 } from "react-icons/lu";
import { DeleteConfirmationDialog } from "@/components/delete-confirmation-dialog";
import {
  SubscriptionFormDialog,
  type SubscriptionInfo,
} from "@/components/subscription-form-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { translateBackendError } from "@/lib/backend-errors";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";
import { cn } from "@/lib/utils";
import type { StoredProxy } from "@/types";

interface SubscriptionEntry {
  id: string;
  subscriptionId: string;
  kind: string;
  entryId: string;
  linkHash: string;
  name: string;
}

export function SubscriptionsPanel({ proxies }: { proxies: StoredProxy[] }) {
  const { t } = useTranslation();
  const [subs, setSubs] = useState<SubscriptionInfo[]>([]);
  const [counts, setCounts] = useState<Record<string, number>>({});
  const [editing, setEditing] = useState<SubscriptionInfo | null>(null);
  const [showForm, setShowForm] = useState(false);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [deleting, setDeleting] = useState<SubscriptionInfo | null>(null);

  const load = useCallback(async () => {
    try {
      const list = await invoke<SubscriptionInfo[]>("subscriptions_list");
      setSubs(list);
      const next: Record<string, number> = {};
      await Promise.all(
        list.map(async (s) => {
          try {
            const entries = await invoke<SubscriptionEntry[]>(
              "subscription_entries",
              { subscriptionId: s.id },
            );
            next[s.id] = entries.length;
          } catch {
            next[s.id] = 0;
          }
        }),
      );
      setCounts(next);
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  }, [t]);

  useEffect(() => {
    void load();
    let unlisten: (() => void) | undefined;
    listen("subscriptions-changed", () => {
      void load();
    })
      .then((fn) => {
        unlisten = fn;
      })
      .catch(() => undefined);
    return () => unlisten?.();
  }, [load]);

  const handleRefresh = async (id: string) => {
    setBusyIds((prev) => new Set(prev).add(id));
    try {
      const result = await invoke<{
        added: number;
        updated: number;
        pruned: number;
        unsupported: number;
        errors: string[];
      }>("subscription_refresh", { id });
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
      await load();
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

  const handleDelete = async (deleteEntries: boolean) => {
    if (!deleting) return;
    try {
      await invoke("subscription_delete", {
        id: deleting.id,
        deleteEntries,
      });
      showSuccessToast(t("subscriptions.deleted"));
      setDeleting(null);
      await load();
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  };

  return (
    <div className="space-y-3 px-1 py-1">
      <div className="flex justify-end">
        <Button
          size="sm"
          onClick={() => {
            setEditing(null);
            setShowForm(true);
          }}
        >
          {t("subscriptions.add")}
        </Button>
      </div>
      {subs.length === 0 ? (
        <p className="py-8 text-center text-sm text-muted-foreground">
          {t("subscriptions.empty")}
        </p>
      ) : (
        subs.map((s) => (
          <div
            key={s.id}
            className="flex items-center justify-between gap-3 rounded-lg border p-3"
          >
            <div className="min-w-0 space-y-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="truncate text-sm font-medium">{s.name}</span>
                <Badge variant="secondary" className="text-xs">
                  {t("subscriptions.entryCount", {
                    count: counts[s.id] ?? 0,
                  })}
                </Badge>
                {s.refreshHours > 0 ? (
                  <Badge variant="secondary" className="text-xs">
                    {t("subscriptions.everyHours", {
                      hours: s.refreshHours,
                    })}
                  </Badge>
                ) : (
                  <Badge variant="secondary" className="text-xs">
                    {t("subscriptions.manual")}
                  </Badge>
                )}
              </div>
              <p className="truncate font-mono text-xs text-muted-foreground">
                {s.url}
              </p>
              {s.lastStatus && (
                <p className="truncate text-xs text-muted-foreground">
                  {s.lastStatus}
                </p>
              )}
            </div>
            <div className="flex shrink-0 items-center gap-1">
              <Button
                variant="ghost"
                size="icon"
                disabled={busyIds.has(s.id)}
                onClick={() => void handleRefresh(s.id)}
                title={t("subscriptions.refreshNow")}
                aria-label={t("subscriptions.refreshNow")}
              >
                {busyIds.has(s.id) ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <LuRefreshCw className="size-4" />
                )}
              </Button>
              <Button
                variant="ghost"
                size="icon"
                disabled={busyIds.has(s.id)}
                onClick={() => {
                  setEditing(s);
                  setShowForm(true);
                }}
                title={t("subscriptions.edit")}
                aria-label={t("subscriptions.edit")}
              >
                <LuPencil className="size-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                disabled={busyIds.has(s.id)}
                onClick={() => setDeleting(s)}
                title={t("subscriptions.delete")}
                aria-label={t("subscriptions.delete")}
                className={cn("text-muted-foreground hover:text-destructive")}
              >
                <LuTrash2 className="size-4" />
              </Button>
            </div>
          </div>
        ))
      )}

      {showForm && (
        <SubscriptionFormDialog
          isOpen={showForm}
          onClose={() => {
            setShowForm(false);
            setEditing(null);
          }}
          editing={editing}
          proxies={proxies}
          onSaved={() => void load()}
        />
      )}

      {deleting && (
        <DeleteConfirmationDialog
          isOpen={Boolean(deleting)}
          onClose={() => setDeleting(null)}
          onConfirm={() => void handleDelete(false)}
          title={t("subscriptions.deleteTitle", { name: deleting.name })}
          description={t("subscriptions.deleteDescription")}
          confirmButtonText={t("subscriptions.deleteKeepEntries")}
          extraActionLabel={t("subscriptions.deleteWithEntries")}
          onExtraAction={() => void handleDelete(true)}
        />
      )}
    </div>
  );
}
