"use client";

import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { LoadingButton } from "@/components/loading-button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { translateBackendError } from "@/lib/backend-errors";
import type { BrowserProfile, GroupWithCount } from "@/types";
import { RippleButton } from "./ui/ripple";

interface GroupMembersDialogProps {
  isOpen: boolean;
  onClose: () => void;
  group: GroupWithCount | null;
  onChanged: () => void;
}

export function GroupMembersDialog({
  isOpen,
  onClose,
  group,
  onChanged,
}: GroupMembersDialogProps) {
  const { t } = useTranslation();
  const [profiles, setProfiles] = useState<BrowserProfile[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [addId, setAddId] = useState<string>("");

  const load = useCallback(async () => {
    setIsLoading(true);
    setError(null);
    try {
      const all = await invoke<BrowserProfile[]>("list_browser_profiles");
      setProfiles(all);
    } catch (err) {
      setError(translateBackendError(t, err));
    } finally {
      setIsLoading(false);
    }
  }, [t]);

  useEffect(() => {
    if (isOpen && group) {
      setAddId("");
      setError(null);
      void load();
    }
  }, [isOpen, group, load]);

  const members = useMemo(
    () => (group ? profiles.filter((p) => p.group_id === group.id) : []),
    [profiles, group],
  );
  const candidates = useMemo(
    () => (group ? profiles.filter((p) => p.group_id !== group.id) : []),
    [profiles, group],
  );

  const handleAdd = useCallback(async () => {
    if (!group || !addId) return;
    setIsSaving(true);
    setError(null);
    try {
      await invoke("assign_profiles_to_group", {
        profileIds: [addId],
        groupId: group.id,
      });
      toast.success(t("groupMembers.addSuccess"));
      setAddId("");
      await load();
      onChanged();
    } catch (err) {
      const msg = translateBackendError(t, err);
      setError(msg);
      toast.error(msg);
    } finally {
      setIsSaving(false);
    }
  }, [group, addId, load, onChanged, t]);

  const handleRemove = useCallback(
    async (profileId: string) => {
      if (!group) return;
      setIsSaving(true);
      setError(null);
      try {
        await invoke("assign_profiles_to_group", {
          profileIds: [profileId],
          groupId: null,
        });
        toast.success(t("groupMembers.removeSuccess"));
        await load();
        onChanged();
      } catch (err) {
        const msg = translateBackendError(t, err);
        setError(msg);
        toast.error(msg);
      } finally {
        setIsSaving(false);
      }
    },
    [group, load, onChanged, t],
  );

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>
            {t("groupMembers.title", { name: group?.name ?? "" })}
          </DialogTitle>
          <DialogDescription>{t("groupMembers.description")}</DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          {isLoading ? (
            <div className="text-sm text-muted-foreground">
              {t("groupMembers.loading")}
            </div>
          ) : (
            <>
              <div className="space-y-2">
                <Label>
                  {t("groupMembers.membersTitle", { count: members.length })}
                </Label>
                {members.length === 0 ? (
                  <p className="text-sm text-muted-foreground">
                    {t("groupMembers.empty")}
                  </p>
                ) : (
                  <ScrollArea className="max-h-[min(10rem,25vh)] w-full overflow-y-auto rounded-md border p-2">
                    <div className="space-y-1">
                      {members.map((p) => (
                        <div
                          key={p.id}
                          className="flex items-center justify-between gap-2 rounded px-2 py-1 text-sm hover:bg-muted"
                        >
                          <span className="min-w-0 flex-1 truncate">
                            {p.name}
                          </span>
                          <RippleButton
                            size="sm"
                            variant="ghost"
                            className="h-6 px-2 text-xs"
                            disabled={isSaving}
                            onClick={() => void handleRemove(p.id)}
                          >
                            {t("groupMembers.removeButton")}
                          </RippleButton>
                        </div>
                      ))}
                    </div>
                  </ScrollArea>
                )}
              </div>

              <div className="space-y-2">
                <Label htmlFor="add-profile">
                  {t("groupMembers.addTitle")}
                </Label>
                <div className="flex gap-2">
                  <Select value={addId} onValueChange={setAddId}>
                    <SelectTrigger className="flex-1">
                      <SelectValue
                        placeholder={t("groupMembers.addPlaceholder")}
                      />
                    </SelectTrigger>
                    <SelectContent>
                      {candidates.map((p) => (
                        <SelectItem key={p.id} value={p.id}>
                          {p.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <LoadingButton
                    isLoading={isSaving}
                    disabled={!addId}
                    onClick={() => void handleAdd()}
                  >
                    {t("groupMembers.addButton")}
                  </LoadingButton>
                </div>
                <p className="text-xs text-muted-foreground">
                  {t("groupMembers.moveHint")}
                </p>
              </div>
            </>
          )}

          {error && (
            <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">
              {error}
            </div>
          )}
        </div>

        <DialogFooter>
          <RippleButton variant="outline" onClick={onClose}>
            {t("common.buttons.close")}
          </RippleButton>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
