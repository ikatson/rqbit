import { useMemo, useCallback, useEffect, useState, useRef } from "react";
import { TorrentListItem } from "../../api-types";
import { TorrentTableRow } from "./TorrentTableRow";
import { useUIStore } from "../../stores/uiStore";
import { Spinner } from "../Spinner";
import { TableHeader } from "./TableHeader";

export type TorrentSortColumn =
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
export type SortDirection = "asc" | "desc";

const SORT_STORAGE_KEY = "rqbit-torrent-sort";

interface StoredSort {
  column: TorrentSortColumn;
  direction: SortDirection;
}

function getDefaultSort(): StoredSort {
  try {
    const stored = localStorage.getItem(SORT_STORAGE_KEY);
    if (stored) {
      const parsed = JSON.parse(stored) as StoredSort;
      if (parsed.column && parsed.direction) {
        return parsed;
      }
    }
  } catch {
    // ignore
  }
  return { column: "id", direction: "desc" };
}

function getSortValue(
  t: TorrentListItem,
  column: TorrentSortColumn,
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

  // Local sorting state
  const [sortColumn, setSortColumnState] = useState<TorrentSortColumn>(
    () => getDefaultSort().column,
  );
  const [sortDirection, setSortDirectionState] = useState<SortDirection>(
    () => getDefaultSort().direction,
  );

  const setSortColumn = useCallback((column: TorrentSortColumn) => {
    setSortColumnState((prevColumn) => {
      setSortDirectionState((prevDir) => {
        const newDir: SortDirection =
          prevColumn === column ? (prevDir === "asc" ? "desc" : "asc") : "desc";
        localStorage.setItem(
          SORT_STORAGE_KEY,
          JSON.stringify({ column, direction: newDir }),
        );
        return newDir;
      });
      return column;
    });
  }, []);

  const sortedTorrents = useMemo(() => {
    if (!torrents) return null;

    return [...torrents].sort((a, b) => {
      const aVal = getSortValue(a, sortColumn);
      const bVal = getSortValue(b, sortColumn);
      const cmp =
        typeof aVal === "string"
          ? aVal.localeCompare(bVal as string)
          : (aVal as number) - (bVal as number);
      return sortDirection === "asc" ? cmp : -cmp;
    });
  }, [torrents, sortColumn, sortDirection]);

  const allSelected = !!(
    torrents &&
    torrents.length > 0 &&
    torrents.every((t) => selectedTorrentIds.has(t.id))
  );
  const someSelected = !!(
    torrents && torrents.some((t) => selectedTorrentIds.has(t.id))
  );

  const handleHeaderCheckbox = () => {
    if (allSelected) {
      clearSelection();
    } else if (torrents) {
      selectAll(torrents.map((t) => t.id));
    }
  };

  const handleSort = (column: TorrentSortColumn) => {
    setSortColumn(column);
  };

  // Store orderedIds in a ref so handleRowClick doesn't need it as a dependency
  const orderedIdsRef = useRef<number[]>([]);
  orderedIdsRef.current = sortedTorrents?.map((t) => t.id) ?? [];

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
    <table className="w-full">
      <thead className="bg-surface-raised sticky top-0 z-10 text-sm">
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
      <tbody className="text-sm">
        {sortedTorrents?.map((torrent) => (
          <TorrentTableRow
            key={torrent.id}
            torrent={torrent}
            isSelected={selectedTorrentIds.has(torrent.id)}
            onRowClick={handleRowClick}
            onCheckboxChange={toggleSelection}
          />
        ))}
      </tbody>
    </table>
  );
};
