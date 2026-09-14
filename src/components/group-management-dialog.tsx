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
import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { GoPlus } from "react-icons/go";
import {
  LuChevronDown,
  LuChevronUp,
  LuFolder,
  LuPencil,
  LuTrash2,
  LuUsers,
} from "react-icons/lu";
import { CreateGroupDialog } from "@/components/create-group-dialog";
import {
  DataTableActionBar,
  DataTableActionBarAction,
  DataTableActionBarSelection,
} from "@/components/data-table-action-bar";
import { DeleteConfirmationDialog } from "@/components/delete-confirmation-dialog";
import { DeleteGroupDialog } from "@/components/delete-group-dialog";
import { EditGroupDialog } from "@/components/edit-group-dialog";
import { GroupMembersDialog } from "@/components/group-members-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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
import { parseBackendError, translateBackendError } from "@/lib/backend-errors";
import { showErrorToast, showSuccessToast } from "@/lib/toast-utils";
import { cn } from "@/lib/utils";
import type { GroupWithCount, ProfileGroup } from "@/types";
import { RippleButton } from "./ui/ripple";

// Group sync status UI removed: group sync is not supported.
// The backend still returns `sync_enabled`/`last_sync` for compatibility,
// but no sync dot, toggle, or bulk-sync action is rendered.

interface GroupManagementDialogProps {
  isOpen: boolean;
  onClose: () => void;
  onGroupManagementComplete: () => void;
  subPage?: boolean;
}

export function GroupManagementDialog({
  isOpen,
  onClose,
  onGroupManagementComplete,
  subPage,
}: GroupManagementDialogProps) {
  const { t } = useTranslation();
  const [groups, setGroups] = useState<GroupWithCount[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Dialog states
  const [createDialogOpen, setCreateDialogOpen] = useState(false);
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [deleteDialogOpen, setDeleteDialogOpen] = useState(false);
  const [bulkDeleteOpen, setBulkDeleteOpen] = useState(false);
  const [isBulkDeleting, setIsBulkDeleting] = useState(false);
  const [selectedGroup, setSelectedGroup] = useState<GroupWithCount | null>(
    null,
  );
  // NOTE: group sync is not supported — the sync toggle, status dot, and
  // bulk-sync action are intentionally commented out below. The backend
  // `sync_enabled` fields remain for compatibility but are hidden from UI.
  // const [groupSyncStatus, setGroupSyncStatus] = ...
  // const [groupSyncErrors, setGroupSyncErrors] = ...
  // const [groupInUse, setGroupInUse] = ...
  // const [isTogglingSync, setIsTogglingSync] = ...

  // Table state
  const [sorting, setSorting] = useState<SortingState>([
    { id: "name", desc: false },
  ]);
  const [rowSelection, setRowSelection] = useState<RowSelectionState>({});

  // Group members assignment state (assign profiles to a group from here)
  const [membersGroup, setMembersGroup] = useState<GroupWithCount | null>(null);
  const [membersOpen, setMembersOpen] = useState(false);

  const loadGroups = useCallback(async () => {
    setIsLoading(true);
    setError(null);
    try {
      const groupList = await invoke<GroupWithCount[]>(
        "get_groups_with_profile_counts",
      );
      setGroups(groupList);
      // Group-sync "in use" check removed: group sync is not supported.
    } catch (err) {
      console.error("Failed to load groups:", err);
      setError(
        err instanceof Error ? err.message : t("groupManagement.loadFailed"),
      );
    } finally {
      setIsLoading(false);
    }
  }, [t]);

  const handleGroupCreated = useCallback(
    (_newGroup: ProfileGroup) => {
      void loadGroups();
      onGroupManagementComplete();
    },
    [loadGroups, onGroupManagementComplete],
  );

  const handleGroupUpdated = useCallback(
    (_updatedGroup: ProfileGroup) => {
      void loadGroups();
      onGroupManagementComplete();
    },
    [loadGroups, onGroupManagementComplete],
  );

  const handleGroupDeleted = useCallback(() => {
    void loadGroups();
    onGroupManagementComplete();
  }, [loadGroups, onGroupManagementComplete]);

  const handleEditGroup = useCallback((group: GroupWithCount) => {
    setSelectedGroup(group);
    setEditDialogOpen(true);
  }, []);

  const handleDeleteGroup = useCallback((group: GroupWithCount) => {
    setSelectedGroup(group);
    setDeleteDialogOpen(true);
  }, []);

  // Group sync toggle removed: group sync is not supported.
  // Previously called `set_group_sync_enabled` here.
  const handleManageMembers = useCallback((group: GroupWithCount) => {
    setMembersGroup(group);
    setMembersOpen(true);
  }, []);

  useEffect(() => {
    if (isOpen) {
      void loadGroups();
    } else {
      // Drop any selection when the dialog closes so the floating
      // action bar (portaled to body) doesn't linger on the page.
      setRowSelection({});
    }
  }, [isOpen, loadGroups]);

  const columns = useMemo<ColumnDef<GroupWithCount>[]>(
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
            disabled={table.getRowModel().rows.length === 0}
          />
        ),
        cell: ({ row }) => (
          <Checkbox
            checked={row.getIsSelected()}
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
        cell: ({ row }) => {
          const group = row.original;
          return (
            <div className="flex min-w-0 items-center gap-2 font-medium">
              <LuFolder className="size-4 shrink-0 text-muted-foreground" />
              <span className="truncate">{group.name}</span>
            </div>
          );
        },
      },
      {
        id: "count",
        size: 80,
        enableSorting: false,
        header: () => t("groupManagement.profilesCol"),
        cell: ({ row }) => (
          <Badge variant="secondary">{row.original.count}</Badge>
        ),
      },
      // Sync column removed: group sync is not supported.
      {
        id: "actions",
        size: 140,
        enableSorting: false,
        header: () => t("common.labels.actions"),
        cell: ({ row }) => {
          const group = row.original;
          return (
            <div className="flex gap-1">
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      handleManageMembers(group);
                    }}
                  >
                    <LuUsers className="size-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>
                  <p>{t("groupManagement.manageMembersTooltip")}</p>
                </TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      handleEditGroup(group);
                    }}
                  >
                    <LuPencil className="size-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>
                  <p>{t("groupManagement.editGroupTooltip")}</p>
                </TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      handleDeleteGroup(group);
                    }}
                  >
                    <LuTrash2 className="size-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>
                  <p>{t("groupManagement.deleteGroupTooltip")}</p>
                </TooltipContent>
              </Tooltip>
            </div>
          );
        },
      },
    ],
    [t, handleManageMembers, handleEditGroup, handleDeleteGroup],
  );

  const table = useReactTable({
    data: groups,
    columns,
    state: { sorting, rowSelection },
    onSortingChange: setSorting,
    onRowSelectionChange: setRowSelection,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getRowId: (row) => row.id,
  });

  const selectedRows = table.getFilteredSelectedRowModel().rows;
  const selectedGroupsForBulk = useMemo(
    () => selectedRows.map((row) => row.original),
    [selectedRows],
  );
  const selectedNames = useMemo(
    () => selectedGroupsForBulk.map((g) => g.name).join(", "),
    [selectedGroupsForBulk],
  );

  const handleBulkDelete = useCallback(async () => {
    if (selectedGroupsForBulk.length === 0) return;
    setIsBulkDeleting(true);
    try {
      const ids = selectedGroupsForBulk.map((g) => g.id);
      const results = await Promise.allSettled(
        ids.map((groupId) => invoke("delete_profile_group", { groupId })),
      );
      const firstRejection = results.find((r) => r.status === "rejected") as
        | PromiseRejectedResult
        | undefined;
      if (firstRejection) {
        showErrorToast(
          parseBackendError(firstRejection.reason)
            ? translateBackendError(t, firstRejection.reason)
            : t("groups.deleteFailed"),
        );
      } else {
        showSuccessToast(t("groups.deleteSuccess"));
      }
      table.toggleAllRowsSelected(false);
      setBulkDeleteOpen(false);
      await loadGroups();
      onGroupManagementComplete();
    } catch (err) {
      console.error("Bulk group delete failed:", err);
      showErrorToast(translateBackendError(t, err));
    } finally {
      setIsBulkDeleting(false);
    }
  }, [selectedGroupsForBulk, table, loadGroups, onGroupManagementComplete, t]);

  // Bulk sync toggle removed: group sync is not supported.

  return (
    <>
      <Dialog open={isOpen} onOpenChange={onClose} subPage={subPage}>
        <DialogContent className="flex max-h-[85vh] max-w-[min(80rem,calc(100%-4rem))] flex-col">
          {!subPage && (
            <DialogHeader>
              <DialogTitle>{t("groups.management")}</DialogTitle>
              <DialogDescription>
                {t("groups.noGroupDescription")}
              </DialogDescription>
            </DialogHeader>
          )}

          <div className="@container flex min-h-0 w-full flex-1 flex-col">
            <div className="flex shrink-0 flex-wrap items-center justify-between gap-2">
              <div className="inline-flex h-7 items-center justify-center gap-1.5 rounded-md bg-accent px-3 text-sm font-medium whitespace-nowrap text-foreground">
                <span>{t("groups.pageTitle")}</span>
                <span className="text-xs text-muted-foreground tabular-nums">
                  {groups.length}
                </span>
              </div>
              <RippleButton
                size="sm"
                onClick={() => {
                  setCreateDialogOpen(true);
                }}
                className="flex shrink-0 items-center gap-2"
                aria-label={t("common.buttons.create")}
              >
                <GoPlus className="size-4" />
                <span className="hidden @2xl:inline">
                  {t("common.buttons.create")}
                </span>
              </RippleButton>
            </div>

            {error && (
              <div className="mt-4 rounded-md bg-destructive/10 p-3 text-sm text-destructive">
                {error}
              </div>
            )}

            {/* Groups list */}
            {isLoading ? (
              <div className="mt-4 text-sm text-muted-foreground">
                {t("common.buttons.loading")}
              </div>
            ) : groups.length === 0 ? (
              <div className="mt-4 text-sm text-muted-foreground">
                {t("groups.noGroupsDescription")}
              </div>
            ) : (
              <FadingScrollArea
                className={cn(
                  "mt-4 min-h-0 flex-1",
                  selectedGroupsForBulk.length > 0 && "pb-16",
                )}
                style={
                  {
                    "--scroll-fade-top-offset": "32px",
                  } as React.CSSProperties
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
                                header.column.id === "name"
                                  ? undefined
                                  : `${header.column.getSize()}px`,
                            }}
                            className={cn(
                              header.column.id === "name" && "max-w-0",
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
                                cell.column.id === "name"
                                  ? undefined
                                  : `${cell.column.getSize()}px`,
                            }}
                            className={cn(
                              cell.column.id === "name" && "max-w-0",
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
          </div>

          {!subPage && (
            <DialogFooter>
              <RippleButton variant="outline" onClick={onClose}>
                {t("common.buttons.close")}
              </RippleButton>
            </DialogFooter>
          )}
        </DialogContent>
      </Dialog>

      {isOpen && (
        <DataTableActionBar table={table}>
          <DataTableActionBarSelection table={table} />
          {/* Bulk sync toggle removed: group sync is not supported. */}
          <DataTableActionBarAction
            tooltip={t("common.buttons.delete")}
            onClick={() => setBulkDeleteOpen(true)}
            size="icon"
            variant="destructive"
            className="border-destructive bg-destructive/50 hover:bg-destructive/70"
          >
            <LuTrash2 />
          </DataTableActionBarAction>
        </DataTableActionBar>
      )}

      <DeleteConfirmationDialog
        isOpen={bulkDeleteOpen}
        onClose={() => {
          if (!isBulkDeleting) setBulkDeleteOpen(false);
        }}
        onConfirm={handleBulkDelete}
        title={t("groupManagement.bulkDelete.title")}
        description={t("groupManagement.bulkDelete.description", {
          count: selectedGroupsForBulk.length,
          names: selectedNames,
        })}
        confirmButtonText={t("groupManagement.bulkDelete.confirmButton")}
        isLoading={isBulkDeleting}
      />

      <CreateGroupDialog
        isOpen={createDialogOpen}
        onClose={() => {
          setCreateDialogOpen(false);
        }}
        onGroupCreated={handleGroupCreated}
      />

      <EditGroupDialog
        isOpen={editDialogOpen}
        onClose={() => {
          setEditDialogOpen(false);
        }}
        group={selectedGroup}
        onGroupUpdated={handleGroupUpdated}
      />

      <DeleteGroupDialog
        isOpen={deleteDialogOpen}
        onClose={() => {
          setDeleteDialogOpen(false);
        }}
        group={selectedGroup}
        onGroupDeleted={handleGroupDeleted}
      />

      <GroupMembersDialog
        isOpen={membersOpen}
        onClose={() => {
          setMembersOpen(false);
          setMembersGroup(null);
        }}
        group={membersGroup}
        onChanged={() => {
          void loadGroups();
          onGroupManagementComplete();
        }}
      />
    </>
  );
}
