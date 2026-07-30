"use client";

import { useTranslation } from "react-i18next";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { RippleButton } from "./ui/ripple";

interface BrowserTermsDialogProps {
  isOpen: boolean;
  onAccepted: () => void;
}

export function BrowserTermsDialog({
  isOpen,
  onAccepted,
}: BrowserTermsDialogProps) {
  const { t } = useTranslation();

  return (
    <Dialog open={isOpen}>
      <DialogContent
        className="sm:max-w-lg"
        onEscapeKeyDown={(e) => {
          e.preventDefault();
        }}
        onPointerDownOutside={(e) => {
          e.preventDefault();
        }}
        onInteractOutside={(e) => {
          e.preventDefault();
        }}
      >
        <DialogHeader>
          <DialogTitle>{t("browserTerms.title")}</DialogTitle>
          <DialogDescription>{t("browserTerms.description")}</DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <p className="text-sm text-muted-foreground">
            {t("browserTerms.body")}
          </p>
        </div>

        <DialogFooter>
          <RippleButton onClick={onAccepted}>
            {t("browserTerms.acknowledgeButton")}
          </RippleButton>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
