"use client";

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { LuChevronLeft, LuChevronRight } from "react-icons/lu";
import { cn } from "@/lib/utils";

export interface TaskWithSchedule {
  id: string;
  name: string;
  enabled: boolean;
  schedule: {
    window_start: string;
    window_end: string;
    timezone: string;
  };
}

interface TaskCalendarProps {
  tasks: TaskWithSchedule[];
  onSelectDay: (date: string) => void;
}

const WEEKDAY_KEYS = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

function dayKey(date: Date): string {
  const y = date.getFullYear();
  const m = String(date.getMonth() + 1).padStart(2, "0");
  const d = String(date.getDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

function startOfMonth(date: Date): Date {
  return new Date(date.getFullYear(), date.getMonth(), 1);
}

function addMonths(date: Date, delta: number): Date {
  return new Date(date.getFullYear(), date.getMonth() + delta, 1);
}

export function TaskCalendar({ tasks, onSelectDay }: TaskCalendarProps) {
  const { t } = useTranslation();
  const [viewMonth, setViewMonth] = useState(() => startOfMonth(new Date()));
  const [selectedDay, setSelectedDay] = useState<Date>(() => new Date());

  const firstDayOffset = startOfMonth(viewMonth).getDay();
  const daysInMonth = new Date(
    viewMonth.getFullYear(),
    viewMonth.getMonth() + 1,
    0,
  ).getDate();

  const cells: Array<Date | null> = [];
  for (let i = 0; i < firstDayOffset; i += 1) {
    cells.push(null);
  }
  for (let d = 1; d <= daysInMonth; d += 1) {
    cells.push(new Date(viewMonth.getFullYear(), viewMonth.getMonth(), d));
  }

  const enabledTasks = tasks.filter((task) => task.enabled);
  const todayKey = dayKey(new Date());

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={() => setViewMonth((m) => addMonths(m, -1))}
            className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            aria-label={t("tasks.calendar.prevMonth")}
          >
            <LuChevronLeft className="size-4" />
          </button>
          <span className="min-w-28 text-center text-sm font-medium">
            {viewMonth.toLocaleDateString(undefined, {
              month: "long",
              year: "numeric",
            })}
          </span>
          <button
            type="button"
            onClick={() => setViewMonth((m) => addMonths(m, 1))}
            className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            aria-label={t("tasks.calendar.nextMonth")}
          >
            <LuChevronRight className="size-4" />
          </button>
        </div>
        <button
          type="button"
          onClick={() => {
            const today = new Date();
            setViewMonth(startOfMonth(today));
            setSelectedDay(today);
          }}
          className="rounded-md px-2 py-1 text-xs text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          {t("tasks.calendar.today")}
        </button>
      </div>

      <div className="grid grid-cols-7 gap-1">
        {WEEKDAY_KEYS.map((w) => (
          <div
            key={w}
            className="text-center text-[11px] font-medium uppercase text-muted-foreground"
          >
            {t(`tasks.calendar.weekdays.${w}`)}
          </div>
        ))}
        {cells.map((date, i) => {
          if (!date) {
            return <div key={`empty-${i}`} className="min-h-16" />;
          }
          const key = dayKey(date);
          const isToday = key === todayKey;
          const isSelected = key === dayKey(selectedDay);
          const hasTasks = enabledTasks.length > 0;
          const firstTask = enabledTasks[0];
          return (
            <button
              key={key}
              type="button"
              onClick={() => {
                setSelectedDay(date);
                onSelectDay(key);
              }}
              className={cn(
                "min-h-16 rounded-md border p-1 text-left transition-colors",
                isSelected
                  ? "border-primary bg-primary/10"
                  : "border-transparent hover:border-border hover:bg-accent",
              )}
            >
              <div className="flex items-center justify-between">
                <span
                  className={cn(
                    "text-xs",
                    isToday && "font-semibold text-primary",
                  )}
                >
                  {date.getDate()}
                </span>
                {hasTasks && (
                  <span className="size-1.5 rounded-full bg-primary" />
                )}
              </div>
              {firstTask && (
                <div className="mt-1 truncate text-[10px] text-muted-foreground">
                  {firstTask.schedule.window_start}–
                  {firstTask.schedule.window_end}
                </div>
              )}
            </button>
          );
        })}
      </div>

      <div className="space-y-1">
        <p className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          {selectedDay.toLocaleDateString(undefined, {
            weekday: "long",
            month: "long",
            day: "numeric",
          })}
        </p>
        {enabledTasks.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            {t("tasks.calendar.noTasks")}
          </p>
        ) : (
          enabledTasks.map((task) => (
            <div
              key={task.id}
              className="flex items-center justify-between rounded-lg border px-3 py-2"
            >
              <span className="text-sm">{task.name}</span>
              <span className="text-xs text-muted-foreground">
                {task.schedule.window_start}–{task.schedule.window_end}
              </span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
