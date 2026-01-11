import { useContext, useEffect, useMemo, useState } from "react";
import { TorrentId, TorrentDetails, TorrentStats, STATE_INITIALIZING, STATE_LIVE } from "../../api-types";
import { APIContext, RefreshTorrentStatsContext } from "../../context";
import { customSetInterval } from "../../helper/customSetInterval";
import { loopUntilSuccess } from "../../helper/loopUntilSuccess";
import { TorrentTableRow } from "./TorrentTableRow";
import { TorrentSortColumn, useUIStore } from "../../stores/uiStore";
import { Spinner } from "../Spinner";
import { SortIcon } from "../SortIcon";
import { torrentDisplayName } from "../../helper/getTorrentDisplayName";

interface TorrentRowDataProps {
  torrent: TorrentId;
  isSelected: boolean;
  onRowClick: () => void;
  onCheckboxChange: () => void;
}

const TorrentRowData: React.FC<TorrentRowDataProps> = ({
  torrent,
  isSelected,
  onRowClick,
  onCheckboxChange,
}) => {
  const [detailsResponse, setDetailsResponse] = useState<TorrentDetails | null>(null);
  const [statsResponse, setStatsResponse] = useState<TorrentStats | null>(null);
  const [forceStatsRefresh, setForceStatsRefresh] = useState(0);
  const API = useContext(APIContext);
  const updateTorrentDataCache = useUIStore((state) => state.updateTorrentDataCache);

  const forceStatsRefreshCallback = () => {
    setForceStatsRefresh((prev) => prev + 1);
  };

  useEffect(() => {
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrent.id).then((details) => {
        setDetailsResponse(details);
        updateTorrentDataCache(torrent.id, { details });
      });
    }, 1000);
  }, [forceStatsRefresh, torrent.id, updateTorrentDataCache]);

  useEffect(() => {
    return customSetInterval(async () => {
      const errorInterval = 10000;
      const liveInterval = 1000;
      const nonLiveInterval = 10000;

      return API.getTorrentStats(torrent.id)
        .then((stats) => {
          setStatsResponse(stats);
          updateTorrentDataCache(torrent.id, { stats });
          return stats;
        })
        .then(
          (stats) => {
            if (stats.state === STATE_INITIALIZING || stats.state === STATE_LIVE) {
              return liveInterval;
            }
            return nonLiveInterval;
          },
          () => errorInterval
        );
    }, 0);
  }, [forceStatsRefresh, torrent.id, updateTorrentDataCache]);

  return (
    <RefreshTorrentStatsContext.Provider value={{ refresh: forceStatsRefreshCallback }}>
      <TorrentTableRow
        id={torrent.id}
        detailsResponse={detailsResponse}
        statsResponse={statsResponse}
        isSelected={isSelected}
        onRowClick={onRowClick}
        onCheckboxChange={onCheckboxChange}
      />
    </RefreshTorrentStatsContext.Provider>
  );
};

interface TorrentTableProps {
  torrents: TorrentId[] | null;
  loading: boolean;
}

export const TorrentTable: React.FC<TorrentTableProps> = ({ torrents, loading }) => {
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);
  const selectTorrent = useUIStore((state) => state.selectTorrent);
  const toggleSelection = useUIStore((state) => state.toggleSelection);
  const selectAll = useUIStore((state) => state.selectAll);
  const clearSelection = useUIStore((state) => state.clearSelection);
  const sortColumn = useUIStore((state) => state.sortColumn);
  const sortDirection = useUIStore((state) => state.sortDirection);
  const setSortColumn = useUIStore((state) => state.setSortColumn);
  const torrentDataCache = useUIStore((state) => state.torrentDataCache);

  const sortedTorrents = useMemo(() => {
    if (!torrents) return null;

    return [...torrents].sort((a, b) => {
      const aData = torrentDataCache.get(a.id);
      const bData = torrentDataCache.get(b.id);

      let cmp = 0;
      switch (sortColumn) {
        case "id":
          cmp = a.id - b.id;
          break;
        case "name": {
          const aName = torrentDisplayName(aData?.details ?? null).toLowerCase();
          const bName = torrentDisplayName(bData?.details ?? null).toLowerCase();
          cmp = aName.localeCompare(bName);
          break;
        }
        case "progress": {
          const aProgress = aData?.stats?.total_bytes
            ? (aData.stats.progress_bytes ?? 0) / aData.stats.total_bytes
            : 0;
          const bProgress = bData?.stats?.total_bytes
            ? (bData.stats.progress_bytes ?? 0) / bData.stats.total_bytes
            : 0;
          cmp = aProgress - bProgress;
          break;
        }
        case "downSpeed": {
          const aSpeed = aData?.stats?.live?.download_speed?.mbps ?? 0;
          const bSpeed = bData?.stats?.live?.download_speed?.mbps ?? 0;
          cmp = aSpeed - bSpeed;
          break;
        }
        case "upSpeed": {
          const aSpeed = aData?.stats?.live?.upload_speed?.mbps ?? 0;
          const bSpeed = bData?.stats?.live?.upload_speed?.mbps ?? 0;
          cmp = aSpeed - bSpeed;
          break;
        }
        case "eta": {
          // ETA: lower is "better" (finishing sooner), Infinity for no ETA
          const getEta = (data: typeof aData) => {
            if (!data?.stats?.live) return Infinity;
            const remaining = (data.stats.total_bytes ?? 0) - (data.stats.progress_bytes ?? 0);
            const speed = data.stats.live.download_speed?.mbps ?? 0;
            if (speed <= 0 || remaining <= 0) return remaining <= 0 ? 0 : Infinity;
            return remaining / (speed * 1024 * 1024);
          };
          const aEta = getEta(aData);
          const bEta = getEta(bData);
          cmp = aEta - bEta;
          break;
        }
        case "peers": {
          const aPeers = aData?.stats?.live?.snapshot.peer_stats?.live ?? 0;
          const bPeers = bData?.stats?.live?.snapshot.peer_stats?.live ?? 0;
          cmp = aPeers - bPeers;
          break;
        }
      }
      return sortDirection === "asc" ? cmp : -cmp;
    });
  }, [torrents, sortColumn, sortDirection, torrentDataCache]);

  const allSelected = !!(torrents && torrents.length > 0 && torrents.every((t) => selectedTorrentIds.has(t.id)));
  const someSelected = !!(torrents && torrents.some((t) => selectedTorrentIds.has(t.id)));

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

  const headerClass =
    "px-2 py-3 font-medium text-gray-700 dark:text-slate-300 cursor-pointer hover:bg-gray-100 dark:hover:bg-slate-700 select-none";

  if (loading) {
    return (
      <div className="flex justify-center items-center h-64">
        <Spinner />
      </div>
    );
  }

  if (!torrents || torrents.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center h-64 text-gray-400 dark:text-slate-500">
        <p className="text-lg">No torrents</p>
        <p className="text-sm">Add a torrent to get started</p>
      </div>
    );
  }

  return (
    <table className="w-full text-sm">
      <thead className="bg-gray-50 dark:bg-slate-800 sticky top-0 z-10">
        <tr className="border-b border-gray-200 dark:border-slate-700">
          <th className="w-8 px-2 py-3">
            <input
              type="checkbox"
              checked={allSelected}
              ref={(el) => {
                if (el) el.indeterminate = someSelected && !allSelected;
              }}
              onChange={handleHeaderCheckbox}
              className="w-4 h-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500 dark:border-slate-600 dark:bg-slate-800"
            />
          </th>
          <th className="w-8 px-1 py-3"></th>
          <th className={`w-12 ${headerClass} text-center`} onClick={() => handleSort("id")}>
            ID
            <SortIcon column="id" sortColumn={sortColumn} sortDirection={sortDirection} />
          </th>
          <th className={`${headerClass} text-left`} onClick={() => handleSort("name")}>
            Name
            <SortIcon column="name" sortColumn={sortColumn} sortDirection={sortDirection} />
          </th>
          <th className={`w-24 ${headerClass} text-center`} onClick={() => handleSort("progress")}>
            Progress
            <SortIcon column="progress" sortColumn={sortColumn} sortDirection={sortDirection} />
          </th>
          <th className={`w-24 ${headerClass} text-right`} onClick={() => handleSort("downSpeed")}>
            Down
            <SortIcon column="downSpeed" sortColumn={sortColumn} sortDirection={sortDirection} />
          </th>
          <th className={`w-24 ${headerClass} text-right`} onClick={() => handleSort("upSpeed")}>
            Up
            <SortIcon column="upSpeed" sortColumn={sortColumn} sortDirection={sortDirection} />
          </th>
          <th className={`w-20 ${headerClass} text-center`} onClick={() => handleSort("eta")}>
            ETA
            <SortIcon column="eta" sortColumn={sortColumn} sortDirection={sortDirection} />
          </th>
          <th className={`w-16 ${headerClass} text-center`} onClick={() => handleSort("peers")}>
            Peers
            <SortIcon column="peers" sortColumn={sortColumn} sortDirection={sortDirection} />
          </th>
        </tr>
      </thead>
      <tbody>
        {sortedTorrents?.map((torrent) => (
          <TorrentRowData
            key={torrent.id}
            torrent={torrent}
            isSelected={selectedTorrentIds.has(torrent.id)}
            onRowClick={() => selectTorrent(torrent.id)}
            onCheckboxChange={() => toggleSelection(torrent.id)}
          />
        ))}
      </tbody>
    </table>
  );
};
