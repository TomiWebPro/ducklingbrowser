"use client";

import {
  type ColumnDef,
  flexRender,
  getCoreRowModel,
  getSortedRowModel,
  type RowSelectionState,
  type SortingState,
  useReactTable,
} from "@tanstack/react-table";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { Loader2 } from "lucide-react";
import {
  type CSSProperties,
  useCallback,
  useEffect,
  useMemo,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import {
  LuChevronDown,
  LuChevronUp,
  LuPencil,
  LuRefreshCw,
  LuTrash2,
} from "react-icons/lu";
import {
  DataTableActionBar,
  DataTableActionBarAction,
  DataTableActionBarSelection,
} from "@/components/data-table-action-bar";
import { DeleteConfirmationDialog } from "@/components/delete-confirmation-dialog";
import {
  SubscriptionFormDialog,
  type SubscriptionInfo,
} from "@/components/subscription-form-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { FadingScrollArea } from "@/components/ui/fading-scroll-area";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
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

interface RefreshResult {
  added: number;
  updated: number;
  pruned: number;
  unsupported: number;
  errors: string[];
}

interface SubscriptionsPanelProps {
  proxies: StoredProxy[];
  showForm: boolean;
  onShowFormChange: (open: boolean) => void;
  editing: SubscriptionInfo | null;
  onEditingChange: (sub: SubscriptionInfo | null) => void;
  onCountChange?: (count: number) => void;
}

export function SubscriptionsPanel({
  proxies,
  showForm,
  onShowFormChange,
  editing,
  onEditingChange,
  onCountChange,
}: SubscriptionsPanelProps) {
  const { t } = useTranslation();
  const [subs, setSubs] = useState<SubscriptionInfo[]>([]);
  const [counts, setCounts] = useState<Record<string, number>>({});
  const [isLoading, setIsLoading] = useState(true);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [deleting, setDeleting] = useState<SubscriptionInfo | null>(null);
  const [isBulkDeleting, setIsBulkDeleting] = useState(false);
  const [showBulkDeleteDialog, setShowBulkDeleteDialog] = useState(false);
  const [sorting, setSorting] = useState<SortingState>([
    { id: "name", desc: false },
  ]);
  const [rowSelection, setRowSelection] = useState<RowSelectionState>({});

  const setShowForm = onShowFormChange;
  const setEditing = onEditingChange;

  const load = useCallback(async () => {
    try {
      const list = await invoke<SubscriptionInfo[]>("subscriptions_list");
      setSubs(list);
      onCountChange?.(list.length);
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
    } finally {
      setIsLoading(false);
    }
  }, [t, onCountChange]);

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

  const proxyName = useCallback(
    (id: string | null | undefined) => {
      if (!id) return t("subscriptions.direct");
      return (
        proxies.find((p) => p.id === id)?.name ?? t("subscriptions.direct")
      );
    },
    [proxies, t],
  );

  const markBusy = useCallback((id: string, busy: boolean) => {
    setBusyIds((prev) => {
      const next = new Set(prev);
      if (busy) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);

  const handleRefresh = useCallback(
    async (id: string) => {
      markBusy(id, true);
      try {
        const result = await invoke<RefreshResult>("subscription_refresh", {
          id,
        });
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
        markBusy(id, false);
      }
    },
    [load, markBusy, t],
  );

  const handleDelete = useCallback(
    async (deleteEntries: boolean) => {
      if (!deleting) return;
      try {
        await invoke("subscription_delete", {
          id: deleting.id,
          deleteEntries,
        });
        showSuccessToast(t("subscriptions.deleted"));
        setDeleting(null);
        await load();
        if (deleteEntries) {
          await emit("stored-proxies-changed");
          await emit("vpn-configs-changed");
        }
      } catch (e) {
        showErrorToast(translateBackendError(t, e));
      }
    },
    [deleting, load, t],
  );

  const columns = useMemo<ColumnDef<SubscriptionInfo>[]>(
    () => [
      {
        id: "select",
        size: 36,
        enableSorting: false,
        header: ({ table }) => (
          <Checkbox
            checked={
              table.getIsAllRowsSelected()
                ? true
                : table.getIsSomeRowsSelected()
                  ? "indeterminate"
                  : false
            }
            onCheckedChange={(value) => {
              table.toggleAllRowsSelected(!!value);
            }}
            aria-label={t("common.aria.selectAll")}
          />
        ),
        cell: ({ row }) => (
          <Checkbox
            checked={row.getIsSelected()}
            disabled={!row.getCanSelect()}
            onCheckedChange={(value) => {
              row.toggleSelected(!!value);
            }}
            aria-label={t("common.aria.selectRow")}
          />
        ),
      },
      {
        accessorKey: "name",
        enableSorting: true,
        sortingFn: "alphanumeric",
        header: ({ column }) => (
          <Button
            variant="ghost"
            onClick={() => {
              column.toggleSorting(column.getIsSorted() === "asc");
            }}
            className="h-auto cursor-pointer justify-start p-0 text-left font-semibold"
          >
            {t("common.labels.name")}
            {column.getIsSorted() === "asc" ? (
              <LuChevronUp className="ml-2 size-4" />
            ) : column.getIsSorted() === "desc" ? (
              <LuChevronDown className="ml-2 size-4" />
            ) : null}
          </Button>
        ),
        cell: ({ row }) => (
          <div className="flex min-w-0 items-center gap-2 font-medium">
            <span className="truncate">{row.original.name}</span>
            <Badge variant="secondary" className="shrink-0 text-xs">
              {t("subscriptions.entryCount", {
                count: counts[row.original.id] ?? 0,
              })}
            </Badge>
          </div>
        ),
      },
      {
        id: "url",
        enableSorting: false,
        header: () => t("subscriptions.columns.url"),
        cell: ({ row }) => (
          <span className="block truncate font-mono text-xs text-muted-foreground">
            {row.original.url}
          </span>
        ),
      },
      {
        id: "schedule",
        size: 96,
        enableSorting: false,
        header: () => t("subscriptions.columns.schedule"),
        cell: ({ row }) => (
          <Badge variant="secondary">
            {row.original.refreshHours > 0
              ? t("subscriptions.everyHours", {
                  hours: row.original.refreshHours,
                })
              : t("subscriptions.manual")}
          </Badge>
        ),
      },
      {
        id: "fetchVia",
        size: 120,
        enableSorting: false,
        header: () => t("subscriptions.form.fetchVia"),
        cell: ({ row }) => (
          <span className="block truncate text-xs text-muted-foreground">
            {proxyName(row.original.useProxyId)}
          </span>
        ),
      },
      {
        id: "status",
        enableSorting: false,
        header: () => t("subscriptions.columns.status"),
        cell: ({ row }) => (
          <span className="block truncate text-xs text-muted-foreground">
            {row.original.lastStatus ?? "—"}
          </span>
        ),
      },
      {
        id: "actions",
        size: 144,
        enableSorting: false,
        header: () => t("common.labels.actions"),
        cell: ({ row }) => {
          const sub = row.original;
          return (
            <div className="flex gap-1">
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busyIds.has(sub.id)}
                    onClick={() => void handleRefresh(sub.id)}
                  >
                    {busyIds.has(sub.id) ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <LuRefreshCw className="size-4" />
                    )}
                  </Button>
                </TooltipTrigger>
                <TooltipContent>
                  <p>{t("subscriptions.refreshNow")}</p>
                </TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busyIds.has(sub.id)}
                    onClick={() => {
                      setEditing(sub);
                      setShowForm(true);
                    }}
                  >
                    <LuPencil className="size-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>
                  <p>{t("subscriptions.edit")}</p>
                </TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <span>
                    <Button
                      variant="ghost"
                      size="sm"
                      disabled={busyIds.has(sub.id)}
                      onClick={() => setDeleting(sub)}
                    >
                      <LuTrash2 className="size-4" />
                    </Button>
                  </span>
                </TooltipTrigger>
                <TooltipContent>
                  <p>{t("subscriptions.delete")}</p>
                </TooltipContent>
              </Tooltip>
            </div>
          );
        },
      },
    ],
    [t, counts, proxyName, busyIds, handleRefresh, setEditing, setShowForm],
  );

  const table = useReactTable({
    data: subs,
    columns,
    state: {
      sorting,
      rowSelection,
    },
    onSortingChange: setSorting,
    onRowSelectionChange: setRowSelection,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getRowId: (row) => row.id,
  });

  const selectedSubs = table
    .getFilteredSelectedRowModel()
    .rows.map((row) => row.original);

  const handleBulkRefresh = useCallback(async () => {
    if (selectedSubs.length === 0) return;
    for (const sub of selectedSubs) markBusy(sub.id, true);
    try {
      const results = await Promise.allSettled(
        selectedSubs.map((sub) =>
          invoke<RefreshResult>("subscription_refresh", { id: sub.id }),
        ),
      );
      const ok = results.filter((r) => r.status === "fulfilled").length;
      const failed = results.length - ok;
      if (ok > 0) {
        showSuccessToast(
          t("subscriptions.bulkRefreshDone", {
            ok,
            total: results.length,
          }),
        );
      }
      if (failed > 0) {
        showErrorToast(
          t("subscriptions.bulkRefreshFailed", {
            failed,
            total: results.length,
          }),
        );
      }
      await load();
      setRowSelection({});
    } finally {
      for (const sub of selectedSubs) markBusy(sub.id, false);
    }
  }, [selectedSubs, load, markBusy, t]);

  const handleBulkDelete = useCallback(async () => {
    if (selectedSubs.length === 0) return;
    setIsBulkDeleting(true);
    try {
      const results = await Promise.allSettled(
        selectedSubs.map((sub) =>
          invoke("subscription_delete", {
            id: sub.id,
            deleteEntries: false,
          }),
        ),
      );
      const failed = results.filter((r) => r.status === "rejected").length;
      const succeeded = results.length - failed;
      if (succeeded > 0) {
        showSuccessToast(t("subscriptions.deleted"));
      }
      if (failed > 0) {
        showErrorToast(t("subscriptions.bulkDeleteFailed"));
      }
      await load();
      setRowSelection({});
    } finally {
      setIsBulkDeleting(false);
      setShowBulkDeleteDialog(false);
    }
  }, [selectedSubs, load, t]);

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4">
      {isLoading ? (
        <div className="text-sm text-muted-foreground">
          {t("subscriptions.loading")}
        </div>
      ) : subs.length === 0 ? (
        <div className="text-sm text-muted-foreground">
          {t("subscriptions.empty")}
        </div>
      ) : (
        <FadingScrollArea
          className={cn("min-h-0 flex-1", selectedSubs.length > 0 && "pb-16")}
          style={
            {
              "--scroll-fade-top-offset": "32px",
            } as CSSProperties
          }
        >
          <Table
            className="w-full table-fixed"
            containerClassName="overflow-visible"
          >
            <TableHeader className="sticky top-0 z-10 bg-background">
              {table.getHeaderGroups().map((headerGroup) => (
                <TableRow key={headerGroup.id}>
                  {headerGroup.headers.map((header) => (
                    <TableHead
                      key={header.id}
                      style={{
                        width:
                          header.column.id === "name" ||
                          header.column.id === "url" ||
                          header.column.id === "status"
                            ? undefined
                            : `${header.column.getSize()}px`,
                      }}
                      className={cn(
                        header.column.id === "name" && "max-w-0",
                        header.column.id === "url" &&
                          "hidden max-w-0 @2xl:table-cell",
                        header.column.id === "status" &&
                          "hidden max-w-0 @2xl:table-cell",
                      )}
                    >
                      {header.isPlaceholder
                        ? null
                        : flexRender(
                            header.column.columnDef.header,
                            header.getContext(),
                          )}
                    </TableHead>
                  ))}
                </TableRow>
              ))}
            </TableHeader>
            <TableBody>
              {table.getRowModel().rows.map((row) => (
                <TableRow
                  key={row.id}
                  data-state={row.getIsSelected() && "selected"}
                >
                  {row.getVisibleCells().map((cell) => (
                    <TableCell
                      key={cell.id}
                      style={{
                        width:
                          cell.column.id === "name" ||
                          cell.column.id === "url" ||
                          cell.column.id === "status"
                            ? undefined
                            : `${cell.column.getSize()}px`,
                      }}
                      className={cn(
                        cell.column.id === "name" && "max-w-0",
                        cell.column.id === "url" &&
                          "hidden max-w-0 @2xl:table-cell",
                        cell.column.id === "status" &&
                          "hidden max-w-0 @2xl:table-cell",
                      )}
                    >
                      {flexRender(
                        cell.column.columnDef.cell,
                        cell.getContext(),
                      )}
                    </TableCell>
                  ))}
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </FadingScrollArea>
      )}

      <DataTableActionBar table={table}>
        <DataTableActionBarSelection table={table} />
        <DataTableActionBarAction
          tooltip={t("subscriptions.refreshSelected")}
          onClick={() => void handleBulkRefresh()}
          size="icon"
        >
          <LuRefreshCw />
        </DataTableActionBarAction>
        <DataTableActionBarAction
          tooltip={t("common.buttons.delete")}
          onClick={() => {
            setShowBulkDeleteDialog(true);
          }}
          size="icon"
          variant="destructive"
          className="border-destructive bg-destructive/50 hover:bg-destructive/70"
        >
          <LuTrash2 />
        </DataTableActionBarAction>
      </DataTableActionBar>

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

      <DeleteConfirmationDialog
        isOpen={showBulkDeleteDialog}
        onClose={() => {
          setShowBulkDeleteDialog(false);
        }}
        onConfirm={handleBulkDelete}
        title={t("subscriptions.bulkDelete.title", {
          count: selectedSubs.length,
        })}
        description={t("subscriptions.bulkDelete.description", {
          count: selectedSubs.length,
          names: selectedSubs.map((s) => s.name).join(", "),
        })}
        confirmButtonText={t("subscriptions.bulkDelete.confirm", {
          count: selectedSubs.length,
        })}
        isLoading={isBulkDeleting}
      />
    </div>
  );
}
