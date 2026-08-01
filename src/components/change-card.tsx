"use client";

import { Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";
import { LuCheck, LuX } from "react-icons/lu";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

export interface ChangeCardData {
  id: string;
  kind: string;
  title: string;
  description: string;
  diff: Record<string, unknown>;
  reversible: boolean;
}

interface ChangeCardProps {
  card: ChangeCardData;
  busy: boolean;
  onConfirm: (id: string) => void;
  onDecline: (id: string) => void;
}

function kindLabel(t: (k: string) => string, kind: string): string {
  switch (kind) {
    case "navigate":
      return t("changeCard.kinds.navigate");
    case "run_browser":
      return t("changeCard.kinds.runBrowser");
    case "profile_update":
      return t("changeCard.kinds.profileUpdate");
    case "proxy":
      return t("changeCard.kinds.proxy");
    default:
      return t("changeCard.kinds.custom");
  }
}

export function ChangeCard({
  card,
  busy,
  onConfirm,
  onDecline,
}: ChangeCardProps) {
  const { t } = useTranslation();
  return (
    <div className="rounded-lg border p-3 space-y-2">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0 space-y-1">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">{card.title}</span>
            <Badge variant="secondary" className="text-xs">
              {kindLabel(t, card.kind)}
            </Badge>
            {card.reversible && (
              <Badge variant="outline" className="text-xs">
                {t("changeCard.reversible")}
              </Badge>
            )}
          </div>
          {card.description && (
            <p className="text-xs text-muted-foreground">{card.description}</p>
          )}
        </div>
      </div>
      <pre className="max-h-40 overflow-auto rounded-md bg-muted/60 p-2 text-[11px] leading-relaxed">
        {JSON.stringify(card.diff, null, 2)}
      </pre>
      <div className="flex justify-end gap-2">
        <Button
          variant="secondary"
          size="sm"
          disabled={busy}
          onClick={() => onDecline(card.id)}
        >
          <LuX className="size-3.5" />
          {t("changeCard.decline")}
        </Button>
        <Button size="sm" disabled={busy} onClick={() => onConfirm(card.id)}>
          {busy ? (
            <Loader2 className="size-3.5 animate-spin" />
          ) : (
            <LuCheck className="size-3.5" />
          )}
          {t("changeCard.confirm")}
        </Button>
      </div>
    </div>
  );
}
