"use client";

import { invoke } from "@tauri-apps/api/core";
import { Loader2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuCalendarDays, LuPencil, LuPlus, LuTrash2 } from "react-icons/lu";
import {
  TaskCalendar,
  type TaskWithSchedule,
} from "@/components/task-calendar";
import { AnimatedSwitch } from "@/components/ui/animated-switch";
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
import { Textarea } from "@/components/ui/textarea";
import { translateBackendError } from "@/lib/backend-errors";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";
import { cn } from "@/lib/utils";

interface TaskDefinition extends TaskWithSchedule {
  description?: string | null;
  mode: string;
  agent_id?: string | null;
  prompt?: string | null;
  steps: unknown[];
  schedule: {
    window_start: string;
    window_end: string;
    timezone: string;
    jitter_minutes: number;
    randomize_daily: boolean;
  };
  same_bucket_rate_limit: boolean;
  enabled: boolean;
  created_at: string;
  updated_at: string;
  next_run_at?: string | null;
  last_run_at?: string | null;
  last_run_status?: string | null;
  last_run_error?: string | null;
  last_run_duration_ms?: number | null;
}

interface McpAgentInfo {
  id: string;
  display_name: string;
  category: string;
  connected: boolean;
  detected: boolean;
}

const TIMEZONES = [
  "UTC",
  "America/New_York",
  "America/Chicago",
  "America/Denver",
  "America/Los_Angeles",
  "America/Sao_Paulo",
  "Europe/London",
  "Europe/Paris",
  "Europe/Berlin",
  "Europe/Madrid",
  "Europe/Moscow",
  "Africa/Cairo",
  "Asia/Dubai",
  "Asia/Karachi",
  "Asia/Kolkata",
  "Asia/Bangkok",
  "Asia/Shanghai",
  "Asia/Tokyo",
  "Asia/Seoul",
  "Asia/Singapore",
  "Australia/Sydney",
  "Pacific/Auckland",
];

interface ScheduledTasksDialogProps {
  isOpen: boolean;
  onClose: () => void;
  subPage?: boolean;
}

function emptyForm() {
  return {
    id: "",
    name: "",
    description: "",
    mode: "live_agent" as string,
    agent_id: "",
    prompt: "",
    window_start: "09:00",
    window_end: "12:00",
    timezone: "UTC",
    jitter_minutes: "30",
    randomize_daily: true,
    same_bucket_rate_limit: true,
    enabled: true,
  };
}

function formatNextRun(iso: string | null | undefined): string {
  if (!iso) return "";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleString();
}

export function ScheduledTasksDialog({
  isOpen,
  onClose,
  subPage,
}: ScheduledTasksDialogProps) {
  const { t } = useTranslation();
  const [tab, setTab] = useState<"tasks" | "calendar">("tasks");
  const [tasks, setTasks] = useState<TaskDefinition[]>([]);
  const [agents, setAgents] = useState<McpAgentInfo[]>([]);
  const [form, setForm] = useState(emptyForm());
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());

  const loadTasks = useCallback(async () => {
    try {
      setTasks(await invoke<TaskDefinition[]>("scheduler_list"));
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  }, [t]);

  useEffect(() => {
    if (isOpen) {
      void loadTasks();
      void invoke<McpAgentInfo[]>("list_mcp_agents")
        .then((all) =>
          setAgents(all.filter((a) => a.category === "cli" && a.detected)),
        )
        .catch(() => {});
    }
  }, [isOpen, loadTasks]);

  const resetForm = () => {
    setForm(emptyForm());
    setEditing(false);
  };

  const startEdit = (task: TaskDefinition) => {
    setForm({
      id: task.id,
      name: task.name,
      description: task.description ?? "",
      mode: task.mode,
      agent_id: task.agent_id ?? "",
      prompt: task.prompt ?? "",
      window_start: task.schedule.window_start,
      window_end: task.schedule.window_end,
      timezone: task.schedule.timezone,
      jitter_minutes: String(task.schedule.jitter_minutes),
      randomize_daily: task.schedule.randomize_daily,
      same_bucket_rate_limit: task.same_bucket_rate_limit,
      enabled: task.enabled,
    });
    setEditing(true);
  };

  const handleSave = async () => {
    if (!form.name.trim()) {
      showErrorToast(t("tasks.form.nameRequired"));
      return;
    }
    if (form.mode === "live_agent" && !form.agent_id) {
      showErrorToast(t("tasks.form.agentRequired"));
      return;
    }
    if (form.mode === "live_agent" && !form.prompt.trim()) {
      showErrorToast(t("tasks.form.promptRequired"));
      return;
    }
    if (form.mode === "macro") {
      showErrorToast(t("tasks.form.macroUnavailable"));
      return;
    }
    setSaving(true);
    try {
      await invoke<TaskDefinition>("scheduler_save", {
        task: {
          id: form.id,
          name: form.name,
          description: form.description || null,
          mode: form.mode,
          profile_id: null,
          agent_id: form.agent_id || null,
          prompt: form.prompt || null,
          steps: [],
          schedule: {
            window_start: form.window_start,
            window_end: form.window_end,
            timezone: form.timezone,
            jitter_minutes: Number(form.jitter_minutes) || 0,
            randomize_daily: form.randomize_daily,
          },
          same_bucket_rate_limit: form.same_bucket_rate_limit,
          enabled: form.enabled,
        },
      });
      showSuccessToast(t("tasks.saved"));
      resetForm();
      await loadTasks();
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (id: string) => {
    setBusyIds((prev) => new Set(prev).add(id));
    try {
      await invoke("scheduler_delete", { id });
      showSuccessToast(t("tasks.deleted"));
      await loadTasks();
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

  const handleToggle = async (task: TaskDefinition, enabled: boolean) => {
    try {
      await invoke<TaskDefinition>("scheduler_set_enabled", {
        id: task.id,
        enabled,
      });
      await loadTasks();
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    }
  };

  const handleSelectDay = (date: string) => {
    resetForm();
    setTab("tasks");
    setForm((prev) => ({
      ...prev,
      name: `${t("tasks.form.prefillName")} ${date}`,
    }));
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose} subPage={subPage}>
      <DialogContent className="max-w-2xl flex flex-col">
        <DialogHeader className="flex flex-row items-center justify-between gap-3">
          <DialogTitle className="flex items-center gap-2">
            <LuCalendarDays className="size-4 text-muted-foreground" />
            {t("tasks.title")}
          </DialogTitle>
          <AnimatedTabs
            value={tab}
            onValueChange={(v) => setTab(v as typeof tab)}
          >
            <AnimatedTabsList
              className={cn(
                "w-full",
                subPage &&
                  "!bg-transparent !p-0 !h-auto !rounded-none justify-start gap-4",
              )}
            >
              <AnimatedTabsTrigger
                value="tasks"
                className={cn(
                  "flex-1",
                  subPage &&
                    "!flex-none !rounded-none !bg-transparent !shadow-none data-[state=active]:!bg-transparent data-[state=active]:!text-foreground data-[state=active]:!shadow-none text-muted-foreground hover:text-foreground !px-1 !py-1 text-xs",
                )}
              >
                {t("tasks.tabTasks")}
              </AnimatedTabsTrigger>
              <AnimatedTabsTrigger
                value="calendar"
                className={cn(
                  "flex-1",
                  subPage &&
                    "!flex-none !rounded-none !bg-transparent !shadow-none data-[state=active]:!bg-transparent data-[state=active]:!text-foreground data-[state=active]:!shadow-none text-muted-foreground hover:text-foreground !px-1 !py-1 text-xs",
                )}
              >
                {t("tasks.tabCalendar")}
              </AnimatedTabsTrigger>
            </AnimatedTabsList>
          </AnimatedTabs>
        </DialogHeader>

        <FadingScrollArea className="max-h-[70vh] flex-1">
          <div className="space-y-6 px-1 py-1">
            <AnimatedTabs value={tab}>
              <AnimatedTabsContent value="tasks" className="mt-0">
                <div className="space-y-4">
                  <div className="flex items-center justify-between">
                    <h3 className="text-sm font-medium">
                      {t("tasks.listTitle")}
                    </h3>
                    <Button size="sm" onClick={resetForm} disabled={!editing}>
                      <LuPlus className="size-3.5" />
                      {t("tasks.newTask")}
                    </Button>
                  </div>

                  {tasks.length === 0 && !editing ? (
                    <p className="text-sm text-muted-foreground">
                      {t("tasks.empty")}
                    </p>
                  ) : (
                    <div className="space-y-2">
                      {tasks.map((task) => (
                        <div
                          key={task.id}
                          className="flex items-center justify-between gap-3 rounded-lg border p-3"
                        >
                          <div className="min-w-0 space-y-1">
                            <div className="flex items-center gap-2">
                              <span className="truncate text-sm font-medium">
                                {task.name}
                              </span>
                              <Badge variant="secondary" className="text-xs">
                                {task.mode === "live_agent"
                                  ? t("tasks.mode.liveAgent")
                                  : t("tasks.mode.macro")}
                              </Badge>
                              {task.last_run_status && (
                                <span
                                  className={cn(
                                    "size-2 rounded-full",
                                    task.last_run_status === "success"
                                      ? "bg-success"
                                      : "bg-destructive",
                                  )}
                                  title={task.last_run_error ?? undefined}
                                />
                              )}
                            </div>
                            <p className="truncate text-xs text-muted-foreground">
                              {formatNextRun(task.next_run_at) ||
                                t("tasks.noNextRun")}
                            </p>
                          </div>
                          <div className="flex shrink-0 items-center gap-1">
                            <AnimatedSwitch
                              checked={task.enabled}
                              onCheckedChange={(checked) =>
                                void handleToggle(task, Boolean(checked))
                              }
                            />
                            <Button
                              variant="ghost"
                              size="icon"
                              onClick={() => startEdit(task)}
                              title={t("tasks.edit")}
                              aria-label={t("tasks.edit")}
                            >
                              <LuPencil className="size-4" />
                            </Button>
                            <Button
                              variant="ghost"
                              size="icon"
                              disabled={busyIds.has(task.id)}
                              onClick={() => void handleDelete(task.id)}
                              title={t("tasks.delete")}
                              aria-label={t("tasks.delete")}
                              className="text-muted-foreground hover:text-destructive"
                            >
                              {busyIds.has(task.id) ? (
                                <Loader2 className="size-4 animate-spin" />
                              ) : (
                                <LuTrash2 className="size-4" />
                              )}
                            </Button>
                          </div>
                        </div>
                      ))}
                    </div>
                  )}

                  {editing && (
                    <div className="space-y-4 rounded-lg border p-4">
                      <h3 className="text-sm font-medium">
                        {t("tasks.form.title")}
                      </h3>
                      <div className="grid grid-cols-2 gap-3">
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.name")}</Label>
                          <Input
                            value={form.name}
                            onChange={(e) =>
                              setForm((f) => ({ ...f, name: e.target.value }))
                            }
                            placeholder={t("tasks.form.namePlaceholder")}
                          />
                        </div>
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.description")}</Label>
                          <Input
                            value={form.description}
                            onChange={(e) =>
                              setForm((f) => ({
                                ...f,
                                description: e.target.value,
                              }))
                            }
                          />
                        </div>
                      </div>

                      <div className="grid grid-cols-2 gap-3">
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.mode")}</Label>
                          <Select
                            value={form.mode}
                            onValueChange={(v) =>
                              setForm((f) => ({ ...f, mode: v }))
                            }
                          >
                            <SelectTrigger>
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              <SelectItem value="live_agent">
                                {t("tasks.mode.liveAgent")}
                              </SelectItem>
                              <SelectItem value="macro" disabled>
                                {t("tasks.mode.macro")}
                              </SelectItem>
                            </SelectContent>
                          </Select>
                        </div>
                        {form.mode === "live_agent" && (
                          <div className="space-y-1.5">
                            <Label>{t("tasks.form.agent")}</Label>
                            <Select
                              value={form.agent_id}
                              onValueChange={(v) =>
                                setForm((f) => ({ ...f, agent_id: v }))
                              }
                            >
                              <SelectTrigger>
                                <SelectValue
                                  placeholder={t("tasks.form.agentPlaceholder")}
                                />
                              </SelectTrigger>
                              <SelectContent>
                                {agents.map((a) => (
                                  <SelectItem key={a.id} value={a.id}>
                                    {a.display_name}
                                  </SelectItem>
                                ))}
                              </SelectContent>
                            </Select>
                          </div>
                        )}
                      </div>

                      {form.mode === "live_agent" && (
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.prompt")}</Label>
                          <Textarea
                            value={form.prompt}
                            onChange={(e) =>
                              setForm((f) => ({
                                ...f,
                                prompt: e.target.value,
                              }))
                            }
                            placeholder={t("tasks.form.promptPlaceholder")}
                            rows={3}
                          />
                        </div>
                      )}
                      {form.mode === "macro" && (
                        <p className="text-xs text-muted-foreground">
                          {t("tasks.form.macroUnavailable")}
                        </p>
                      )}

                      <div className="grid grid-cols-3 gap-3">
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.windowStart")}</Label>
                          <Input
                            type="time"
                            value={form.window_start}
                            onChange={(e) =>
                              setForm((f) => ({
                                ...f,
                                window_start: e.target.value,
                              }))
                            }
                          />
                        </div>
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.windowEnd")}</Label>
                          <Input
                            type="time"
                            value={form.window_end}
                            onChange={(e) =>
                              setForm((f) => ({
                                ...f,
                                window_end: e.target.value,
                              }))
                            }
                          />
                        </div>
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.timezone")}</Label>
                          <Select
                            value={form.timezone}
                            onValueChange={(v) =>
                              setForm((f) => ({ ...f, timezone: v }))
                            }
                          >
                            <SelectTrigger>
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              {TIMEZONES.map((tz) => (
                                <SelectItem key={tz} value={tz}>
                                  {tz}
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                        </div>
                      </div>

                      <div className="grid grid-cols-2 gap-3">
                        <div className="space-y-1.5">
                          <Label>{t("tasks.form.jitter")}</Label>
                          <Input
                            type="number"
                            min={0}
                            value={form.jitter_minutes}
                            onChange={(e) =>
                              setForm((f) => ({
                                ...f,
                                jitter_minutes: e.target.value,
                              }))
                            }
                          />
                        </div>
                        <div className="flex flex-col justify-end gap-2 pb-0.5">
                          <div className="flex items-center gap-2">
                            <Checkbox
                              id="randomize-daily"
                              checked={form.randomize_daily}
                              onCheckedChange={(checked) =>
                                setForm((f) => ({
                                  ...f,
                                  randomize_daily: Boolean(checked),
                                }))
                              }
                            />
                            <Label
                              htmlFor="randomize-daily"
                              className="font-medium"
                            >
                              {t("tasks.form.randomizeDaily")}
                            </Label>
                          </div>
                          <div className="flex items-center gap-2">
                            <Checkbox
                              id="same-bucket"
                              checked={form.same_bucket_rate_limit}
                              onCheckedChange={(checked) =>
                                setForm((f) => ({
                                  ...f,
                                  same_bucket_rate_limit: Boolean(checked),
                                }))
                              }
                            />
                            <Label
                              htmlFor="same-bucket"
                              className="font-medium"
                            >
                              {t("tasks.form.sameBucket")}
                            </Label>
                          </div>
                        </div>
                      </div>

                      <div className="flex items-center justify-between">
                        <div className="flex items-center gap-2">
                          <AnimatedSwitch
                            id="task-enabled"
                            checked={form.enabled}
                            onCheckedChange={(checked) =>
                              setForm((f) => ({
                                ...f,
                                enabled: Boolean(checked),
                              }))
                            }
                          />
                          <Label htmlFor="task-enabled" className="font-medium">
                            {t("tasks.form.enabled")}
                          </Label>
                        </div>
                        <div className="flex gap-2">
                          <Button variant="secondary" onClick={resetForm}>
                            {t("tasks.form.cancel")}
                          </Button>
                          <Button
                            onClick={() => void handleSave()}
                            disabled={saving}
                          >
                            {saving ? (
                              <Loader2 className="size-4 animate-spin" />
                            ) : (
                              <LuPlus className="size-4" />
                            )}
                            {t("tasks.form.save")}
                          </Button>
                        </div>
                      </div>
                    </div>
                  )}
                </div>
              </AnimatedTabsContent>

              <AnimatedTabsContent value="calendar" className="mt-0">
                <TaskCalendar tasks={tasks} onSelectDay={handleSelectDay} />
              </AnimatedTabsContent>
            </AnimatedTabs>
          </div>
        </FadingScrollArea>
      </DialogContent>
    </Dialog>
  );
}
