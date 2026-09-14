"use client";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { LuDownload, LuRefreshCw, LuX } from "react-icons/lu";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FadingScrollArea } from "@/components/ui/fading-scroll-area";
import { Progress } from "@/components/ui/progress";
import { translateBackendError } from "@/lib/backend-errors";
import { showErrorToast } from "@/lib/toast-utils";
import { RippleButton } from "./ui/ripple";

interface DownloadProgress {
  browser: string;
  version: string;
  downloaded_bytes: number;
  total_bytes?: number;
  percentage: number;
  speed_bytes_per_sec: number;
  eta_seconds?: number;
  stage: string;
}

interface DnsCacheStatus {
  level: string;
  display_name: string;
  entry_count: number;
  file_size_bytes: number;
  last_updated?: number | null;
  is_fresh: boolean;
  is_cached: boolean;
}

function formatBytes(bytes: number): string {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  const i = Math.min(
    units.length - 1,
    Math.floor(Math.log(bytes) / Math.log(1024)),
  );
  return `${(bytes / 1024 ** i).toFixed(1)} ${units[i]}`;
}

interface DownloadsDialogProps {
  isOpen: boolean;
  onClose: () => void;
  subPage?: boolean;
}

export function DownloadsDialog({
  isOpen,
  onClose,
  subPage,
}: DownloadsDialogProps) {
  const { t } = useTranslation();
  const [active, setActive] = useState<Record<string, DownloadProgress>>({});
  const [geoip, setGeoip] = useState<{
    stage: string;
    percentage: number;
    message: string;
  } | null>(null);
  const [browsers, setBrowsers] = useState<string[]>([]);
  const [downloaded, setDownloaded] = useState<Record<string, string[]>>({});
  const [dns, setDns] = useState<DnsCacheStatus[]>([]);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const supported = await invoke<string[]>("get_supported_browsers");
      setBrowsers(supported);
      const dl: Record<string, string[]> = {};
      await Promise.all(
        supported.map(async (b) => {
          try {
            dl[b] = await invoke<string[]>("get_downloaded_browser_versions", {
              browserStr: b,
            });
          } catch {
            dl[b] = [];
          }
        }),
      );
      setDownloaded(dl);
      try {
        setDns(
          await invoke<DnsCacheStatus[]>("get_dns_blocklist_cache_status"),
        );
      } catch {
        setDns([]);
      }
    } catch (e) {
      showErrorToast(translateBackendError(t, e));
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    if (isOpen) void load();
  }, [isOpen, load]);

  useEffect(() => {
    let unlistenBrowser: (() => void) | undefined;
    let unlistenGeoip: (() => void) | undefined;
    const setup = async () => {
      unlistenBrowser = await listen<DownloadProgress>(
        "download-progress",
        (event) => {
          const p = event.payload;
          const key = `${p.browser}@${p.version}`;
          if (
            p.stage === "completed" ||
            p.stage === "cancelled" ||
            p.stage === "error"
          ) {
            setActive((prev) => {
              const next = { ...prev };
              delete next[key];
              return next;
            });
            void load();
          } else {
            setActive((prev) => ({ ...prev, [key]: p }));
          }
        },
      );
      unlistenGeoip = await listen<{
        stage: string;
        percentage: number;
        message: string;
      }>("geoip-download-progress", (event) => {
        const p = event.payload;
        if (p.stage === "completed" || p.stage === "error") {
          setGeoip(null);
        } else {
          setGeoip(p);
        }
      });
    };
    void setup();
    return () => {
      unlistenBrowser?.();
      unlistenGeoip?.();
    };
  }, [load]);

  const cancel = (browser: string, version: string) => {
    invoke("cancel_download", { browserStr: browser, version }).catch((e) => {
      showErrorToast(translateBackendError(t, e));
    });
  };

  const activeList = Object.values(active);

  return (
    <Dialog open={isOpen} onOpenChange={onClose} subPage={subPage}>
      <DialogContent className="flex max-h-[85vh] max-w-2xl flex-col">
        {!subPage && (
          <DialogHeader>
            <DialogTitle>{t("downloads.title")}</DialogTitle>
            <DialogDescription>{t("downloads.description")}</DialogDescription>
          </DialogHeader>
        )}
        <FadingScrollArea className="min-h-0 flex-1">
          <div className="space-y-6 px-1 py-1">
            <section className="space-y-3">
              <div className="flex items-center justify-between">
                <h3 className="text-base font-semibold">
                  {t("downloads.activeTitle")}
                </h3>
                <RippleButton
                  size="sm"
                  variant="outline"
                  className="h-7 px-2 text-xs"
                  onClick={() => void load()}
                  disabled={loading}
                >
                  <LuRefreshCw
                    className={
                      loading ? "mr-1 size-3 animate-spin" : "mr-1 size-3"
                    }
                  />
                  {t("downloads.refresh")}
                </RippleButton>
              </div>
              {activeList.length === 0 && !geoip ? (
                <p className="text-sm text-muted-foreground">
                  {t("downloads.noActive")}
                </p>
              ) : (
                <div className="space-y-2">
                  {geoip && (
                    <div className="rounded-lg border p-3">
                      <div className="flex items-center justify-between gap-2">
                        <p className="text-sm font-medium">
                          {t("downloads.geoipLabel")}
                        </p>
                        <Badge variant="secondary">{geoip.stage}</Badge>
                      </div>
                      <Progress value={geoip.percentage} className="mt-2" />
                      <p className="mt-1 text-xs text-muted-foreground">
                        {geoip.message}
                      </p>
                    </div>
                  )}
                  {activeList.map((p) => (
                    <div
                      key={`${p.browser}@${p.version}`}
                      className="rounded-lg border p-3"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <p className="truncate text-sm font-medium">
                          {p.browser} {p.version}
                        </p>
                        <div className="flex shrink-0 items-center gap-2">
                          <Badge variant="secondary">{p.stage}</Badge>
                          <RippleButton
                            size="sm"
                            variant="ghost"
                            className="h-6 px-2 text-xs"
                            onClick={() => cancel(p.browser, p.version)}
                          >
                            <LuX className="size-3" />
                          </RippleButton>
                        </div>
                      </div>
                      <Progress value={p.percentage} className="mt-2" />
                      <p className="mt-1 text-xs text-muted-foreground tabular-nums">
                        {p.percentage.toFixed(1)}% •{" "}
                        {formatBytes(p.speed_bytes_per_sec)}/s
                        {p.total_bytes
                          ? ` • ${formatBytes(p.downloaded_bytes)} / ${formatBytes(p.total_bytes)}`
                          : ""}
                      </p>
                    </div>
                  ))}
                </div>
              )}
            </section>

            <section className="space-y-3">
              <h3 className="text-base font-semibold">
                {t("downloads.browsersTitle")}
              </h3>
              {browsers.length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  {loading ? t("downloads.loading") : t("downloads.empty")}
                </p>
              ) : (
                <div className="space-y-2">
                  {browsers.map((b) => (
                    <div key={b} className="rounded-lg border p-3">
                      <div className="flex items-center gap-2">
                        <LuDownload className="size-4 shrink-0 text-muted-foreground" />
                        <p className="text-sm font-medium capitalize">{b}</p>
                      </div>
                      {(downloaded[b] ?? []).length === 0 ? (
                        <p className="mt-1 text-xs text-muted-foreground">
                          {t("downloads.noneDownloaded")}
                        </p>
                      ) : (
                        <div className="mt-2 flex flex-wrap gap-1.5">
                          {(downloaded[b] ?? []).map((v) => (
                            <Badge key={v} variant="secondary">
                              {v}
                            </Badge>
                          ))}
                        </div>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </section>

            <section className="space-y-3">
              <h3 className="text-base font-semibold">
                {t("downloads.dnsTitle")}
              </h3>
              {dns.length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  {t("downloads.dnsEmpty")}
                </p>
              ) : (
                <div className="space-y-2">
                  {dns.map((d) => (
                    <div key={d.level} className="rounded-lg border p-3">
                      <div className="flex items-center justify-between gap-2">
                        <p className="text-sm font-medium">{d.display_name}</p>
                        <Badge variant={d.is_cached ? "secondary" : "outline"}>
                          {d.is_cached
                            ? t("downloads.cached")
                            : t("downloads.notCached")}
                        </Badge>
                      </div>
                      <p className="mt-1 text-xs text-muted-foreground tabular-nums">
                        {d.entry_count.toLocaleString()}{" "}
                        {t("downloads.entries")} •{" "}
                        {formatBytes(d.file_size_bytes)}
                        {d.last_updated
                          ? ` • ${new Date(d.last_updated * 1000).toLocaleString()}`
                          : ""}
                      </p>
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
