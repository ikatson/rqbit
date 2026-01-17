import { useMemo, useCallback, useEffect, useState, useRef } from "react";
import { Virtuoso } from "react-virtuoso";
import { TorrentListItem } from "../../api-types";
import { TorrentTableRow } from "./TorrentTableRow";
import { useUIStore } from "../../stores/uiStore";
import { Spinner } from "../Spinner";
import { TableHeader } from "./TableHeader";
import { isTorrentVisible, SortDirection } from "../../helper/torrentFilters";

// Extended sort columns for table view (includes columns not in card view)
export type TableSortColumn =
  | "id"
  | "name"
  | "size"
  | "progress"
  | "downloadedBytes"
  | "downSpeed"
  | "upSpeed"
  | "uploadedBytes"
  | "eta"
  | "peers";

const DEFAULT_SORT_COLUMN: TableSortColumn = "id";
const DEFAULT_SORT_DIRECTION: SortDirection = "desc";

function getTableSortValue(
  t: TorrentListItem,
  column: TableSortColumn,
): number | string {
  switch (column) {
    case "id":
      return t.id;
    case "name":
      return (t.name ?? "").toLowerCase();
    case "size":
      return t.stats?.total_bytes ?? 0;
    case "progress":
      return t.stats?.total_bytes
        ? (t.stats.progress_bytes ?? 0) / t.stats.total_bytes
        : 0;
    case "downloadedBytes":
      return t.stats?.progress_bytes ?? 0;
    case "downSpeed":
      return t.stats?.live?.download_speed?.mbps ?? 0;
    case "upSpeed":
      return t.stats?.live?.upload_speed?.mbps ?? 0;
    case "uploadedBytes":
      return t.stats?.live?.snapshot.uploaded_bytes ?? 0;
    case "eta": {
      if (!t.stats?.live) return Infinity;
      const remaining =
        (t.stats.total_bytes ?? 0) - (t.stats.progress_bytes ?? 0);
      const speed = t.stats.live.download_speed?.mbps ?? 0;
      if (speed <= 0 || remaining <= 0) return remaining <= 0 ? 0 : Infinity;
      return remaining / (speed * 1024 * 1024);
    }
    case "peers":
      return t.stats?.live?.snapshot.peer_stats?.live ?? 0;
  }
}

interface TorrentTableProps {
  torrents: TorrentListItem[] | null;
  loading: boolean;
}

export const TorrentTable: React.FC<TorrentTableProps> = ({
  torrents,
  loading,
}) => {
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);
  const selectTorrent = useUIStore((state) => state.selectTorrent);
  const toggleSelection = useUIStore((state) => state.toggleSelection);
  const selectRange = useUIStore((state) => state.selectRange);
  const selectRelative = useUIStore((state) => state.selectRelative);
  const selectAll = useUIStore((state) => state.selectAll);
  const clearSelection = useUIStore((state) => state.clearSelection);
  const searchQuery = useUIStore((state) => state.searchQuery);
  const statusFilter = useUIStore((state) => state.statusFilter);

  const normalizedQuery = searchQuery.toLowerCase().trim();

  // Local sorting state
  const [sortColumn, setSortColumnState] =
    useState<TableSortColumn>(DEFAULT_SORT_COLUMN);
  const [sortDirection, setSortDirectionState] = useState<SortDirection>(
    DEFAULT_SORT_DIRECTION,
  );

  const setSortColumn = useCallback((column: TableSortColumn) => {
    setSortColumnState((prevColumn) => {
      setSortDirectionState((prevDir) => {
        const newDir: SortDirection =
          prevColumn === column ? (prevDir === "asc" ? "desc" : "asc") : "desc";
        return newDir;
      });
      return column;
    });
  }, []);

  // Sort and filter torrents for virtualization
  const filteredTorrents = useMemo(() => {
    if (!torrents) return null;

    return [...torrents]
      .filter((t) => isTorrentVisible(t, normalizedQuery, statusFilter))
      .sort((a, b) => {
        const aVal = getTableSortValue(a, sortColumn);
        const bVal = getTableSortValue(b, sortColumn);
        const cmp =
          typeof aVal === "string"
            ? aVal.localeCompare(bVal as string)
            : (aVal as number) - (bVal as number);
        return sortDirection === "asc" ? cmp : -cmp;
      });
  }, [torrents, normalizedQuery, statusFilter, sortColumn, sortDirection]);

  // Compute visible IDs for keyboard navigation
  const visibleTorrentIds = useMemo(() => {
    if (!filteredTorrents) return [];
    return filteredTorrents.map((t) => t.id);
  }, [filteredTorrents]);

  const allSelected = !!(
    visibleTorrentIds.length > 0 &&
    visibleTorrentIds.every((id) => selectedTorrentIds.has(id))
  );
  const someSelected = visibleTorrentIds.some((id) =>
    selectedTorrentIds.has(id),
  );

  const handleHeaderCheckbox = () => {
    if (allSelected) {
      clearSelection();
    } else {
      selectAll(visibleTorrentIds);
    }
  };

  const handleSort = (column: TableSortColumn) => {
    setSortColumn(column);
  };

  // Store orderedIds in a ref so handleRowClick doesn't need it as a dependency
  // Use visibleTorrentIds for navigation (skips hidden rows)
  const orderedIdsRef = useRef<number[]>([]);
  orderedIdsRef.current = visibleTorrentIds;

  // Handle keyboard navigation
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Only handle if no input is focused
      const activeElement = document.activeElement;
      if (
        activeElement &&
        (activeElement.tagName === "INPUT" ||
          activeElement.tagName === "TEXTAREA" ||
          activeElement.tagName === "SELECT")
      ) {
        return;
      }

      if (e.key === "ArrowDown") {
        e.preventDefault();
        selectRelative("down", orderedIdsRef.current);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        selectRelative("up", orderedIdsRef.current);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [selectRelative]);

  // Row click handler - stable because it reads from ref
  const handleRowClick = useCallback(
    (id: number, e: React.MouseEvent) => {
      if (e.shiftKey) {
        e.preventDefault();
        selectRange(id, orderedIdsRef.current);
      } else {
        selectTorrent(id);
      }
    },
    [selectRange, selectTorrent],
  );

  // Item renderer for react-virtuoso
  const itemContent = useCallback(
    (index: number) => {
      const torrent = filteredTorrents![index];
      return (
        <TorrentTableRow
          key={torrent.id}
          torrent={torrent}
          isSelected={selectedTorrentIds.has(torrent.id)}
          onRowClick={handleRowClick}
          onCheckboxChange={toggleSelection}
        />
      );
    },
    [filteredTorrents, selectedTorrentIds, handleRowClick, toggleSelection],
  );

  if (loading) {
    return (
      <div className="flex justify-center items-center h-64">
        <Spinner />
      </div>
    );
  }

  if (!torrents || torrents.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center h-64 text-tertiary">
        <p className="text-lg">No torrents</p>
        <p className="">Add a torrent to get started</p>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* Header */}
      <table className="w-full table-fixed">
        <thead className="bg-surface-raised text-sm">
          <tr className="border-b border-divider">
            <th className="w-8 px-2 py-3">
              <input
                type="checkbox"
                checked={allSelected}
                ref={(el) => {
                  if (el) el.indeterminate = someSelected && !allSelected;
                }}
                onChange={handleHeaderCheckbox}
                className="w-4 h-4 rounded border-divider-strong bg-surface text-primary focus:ring-primary"
              />
            </th>
            <th className="w-8 px-1 py-3"></th>
            <TableHeader
              column="id"
              label="ID"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-12"
              align="center"
            />
            <TableHeader
              column="name"
              label="Name"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              align="left"
            />
            <TableHeader
              column="size"
              label="Size"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-20"
              align="right"
            />
            <TableHeader
              column="progress"
              label="Progress"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-24"
              align="center"
            />
            <TableHeader
              column="downloadedBytes"
              label="Recv"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-20"
              align="right"
            />
            <TableHeader
              column="downSpeed"
              label="↓ Speed"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-20"
              align="right"
            />
            <TableHeader
              column="upSpeed"
              label="↑ Speed"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-20"
              align="right"
            />
            <TableHeader
              column="uploadedBytes"
              label="Sent"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-20"
              align="right"
            />
            <TableHeader
              column="eta"
              label="ETA"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-20"
              align="center"
            />
            <TableHeader
              column="peers"
              label="Peers"
              sortColumn={sortColumn}
              sortDirection={sortDirection}
              onSort={handleSort}
              className="w-16"
              align="center"
            />
          </tr>
        </thead>
      </table>
      {/* Virtualized body */}
      <div className="flex-1 min-h-0">
        <Virtuoso
          totalCount={filteredTorrents?.length ?? 0}
          itemContent={itemContent}
        />
      </div>
    </div>
  );
};
