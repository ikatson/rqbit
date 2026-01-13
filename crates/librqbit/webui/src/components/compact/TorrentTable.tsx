import { useMemo, useCallback, useEffect } from "react";
import { TorrentListItem, STATE_INITIALIZING } from "../../api-types";
import { RefreshTorrentStatsContext } from "../../context";
import { TorrentTableRow } from "./TorrentTableRow";
import { TorrentSortColumn, useUIStore } from "../../stores/uiStore";
import { Spinner } from "../Spinner";
import { TableHeader } from "./TableHeader";
import { torrentDisplayName } from "../../helper/getTorrentDisplayName";
import { useTorrentStore } from "../../stores/torrentStore";

interface TorrentRowDataProps {
  torrent: TorrentListItem;
  isSelected: boolean;
  onRowClick: (e: React.MouseEvent) => void;
  onCheckboxChange: () => void;
}

const TorrentRowData: React.FC<TorrentRowDataProps> = ({
  torrent,
  isSelected,
  onRowClick,
  onCheckboxChange,
}) => {
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  const forceStatsRefreshCallback = () => {
    refreshTorrents();
  };

  // Create synthetic details for display (files not included in list response)
  const syntheticDetails = {
    name: torrent.name,
    info_hash: torrent.info_hash,
    files: [],
    total_pieces: torrent.total_pieces,
    output_folder: torrent.output_folder,
  };

  return (
    <RefreshTorrentStatsContext.Provider
      value={{ refresh: forceStatsRefreshCallback }}
    >
      <TorrentTableRow
        id={torrent.id}
        detailsResponse={syntheticDetails}
        statsResponse={torrent.stats ?? null}
        isSelected={isSelected}
        onRowClick={onRowClick}
        onCheckboxChange={onCheckboxChange}
      />
    </RefreshTorrentStatsContext.Provider>
  );
};

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
  const sortColumn = useUIStore((state) => state.sortColumn);
  const sortDirection = useUIStore((state) => state.sortDirection);
  const setSortColumn = useUIStore((state) => state.setSortColumn);

  const sortedTorrents = useMemo(() => {
    if (!torrents) return null;

    return [...torrents].sort((a, b) => {
      let cmp = 0;
      switch (sortColumn) {
        case "id":
          cmp = a.id - b.id;
          break;
        case "name": {
          const aName = (a.name ?? "").toLowerCase();
          const bName = (b.name ?? "").toLowerCase();
          cmp = aName.localeCompare(bName);
          break;
        }
        case "size": {
          const aSize = a.stats?.total_bytes ?? 0;
          const bSize = b.stats?.total_bytes ?? 0;
          cmp = aSize - bSize;
          break;
        }
        case "progress": {
          const aProgress = a.stats?.total_bytes
            ? (a.stats.progress_bytes ?? 0) / a.stats.total_bytes
            : 0;
          const bProgress = b.stats?.total_bytes
            ? (b.stats.progress_bytes ?? 0) / b.stats.total_bytes
            : 0;
          cmp = aProgress - bProgress;
          break;
        }
        case "downloadedBytes": {
          const aBytes = a.stats?.progress_bytes ?? 0;
          const bBytes = b.stats?.progress_bytes ?? 0;
          cmp = aBytes - bBytes;
          break;
        }
        case "downSpeed": {
          const aSpeed = a.stats?.live?.download_speed?.mbps ?? 0;
          const bSpeed = b.stats?.live?.download_speed?.mbps ?? 0;
          cmp = aSpeed - bSpeed;
          break;
        }
        case "upSpeed": {
          const aSpeed = a.stats?.live?.upload_speed?.mbps ?? 0;
          const bSpeed = b.stats?.live?.upload_speed?.mbps ?? 0;
          cmp = aSpeed - bSpeed;
          break;
        }
        case "uploadedBytes": {
          const aBytes = a.stats?.live?.snapshot.uploaded_bytes ?? 0;
          const bBytes = b.stats?.live?.snapshot.uploaded_bytes ?? 0;
          cmp = aBytes - bBytes;
          break;
        }
        case "eta": {
          // ETA: lower is "better" (finishing sooner), Infinity for no ETA
          const getEta = (t: TorrentListItem) => {
            if (!t.stats?.live) return Infinity;
            const remaining =
              (t.stats.total_bytes ?? 0) - (t.stats.progress_bytes ?? 0);
            const speed = t.stats.live.download_speed?.mbps ?? 0;
            if (speed <= 0 || remaining <= 0)
              return remaining <= 0 ? 0 : Infinity;
            return remaining / (speed * 1024 * 1024);
          };
          const aEta = getEta(a);
          const bEta = getEta(b);
          cmp = aEta - bEta;
          break;
        }
        case "peers": {
          const aPeers = a.stats?.live?.snapshot.peer_stats?.live ?? 0;
          const bPeers = b.stats?.live?.snapshot.peer_stats?.live ?? 0;
          cmp = aPeers - bPeers;
          break;
        }
      }
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

  // Get ordered IDs for keyboard navigation
  const orderedIds = useMemo(() => {
    return sortedTorrents?.map((t) => t.id) ?? [];
  }, [sortedTorrents]);

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
        selectRelative("down", orderedIds);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        selectRelative("up", orderedIds);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [orderedIds, selectRelative]);

  // Row click handler with shift support
  const handleRowClick = useCallback(
    (id: number, e: React.MouseEvent) => {
      if (e.shiftKey) {
        e.preventDefault();

        selectRange(id, orderedIds);
      } else {
        selectTorrent(id);
      }
    },
    [orderedIds, selectRange, selectTorrent],
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
      <div className="flex flex-col items-center justify-center h-64 text-text-tertiary">
        <p className="text-lg">No torrents</p>
        <p className="">Add a torrent to get started</p>
      </div>
    );
  }

  return (
    <table className="w-full">
      <thead className="bg-surface-raised sticky top-0 z-10 text-sm">
        <tr className="border-b border-border">
          <th className="w-8 px-2 py-3">
            <input
              type="checkbox"
              checked={allSelected}
              ref={(el) => {
                if (el) el.indeterminate = someSelected && !allSelected;
              }}
              onChange={handleHeaderCheckbox}
              className="w-4 h-4 rounded border-border-strong bg-surface text-primary focus:ring-primary"
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
          <TorrentRowData
            key={torrent.id}
            torrent={torrent}
            isSelected={selectedTorrentIds.has(torrent.id)}
            onRowClick={(e) => handleRowClick(torrent.id, e)}
            onCheckboxChange={() => toggleSelection(torrent.id)}
          />
        ))}
      </tbody>
    </table>
  );
};
