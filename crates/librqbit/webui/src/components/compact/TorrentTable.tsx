import { useContext, useEffect, useState } from "react";
import { TorrentId, TorrentDetails, TorrentStats, STATE_INITIALIZING, STATE_LIVE } from "../../api-types";
import { APIContext, RefreshTorrentStatsContext } from "../../context";
import { customSetInterval } from "../../helper/customSetInterval";
import { loopUntilSuccess } from "../../helper/loopUntilSuccess";
import { TorrentTableRow } from "./TorrentTableRow";
import { useUIStore } from "../../stores/uiStore";
import { Spinner } from "../Spinner";

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

  const forceStatsRefreshCallback = () => {
    setForceStatsRefresh((prev) => prev + 1);
  };

  useEffect(() => {
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrent.id).then(setDetailsResponse);
    }, 1000);
  }, [forceStatsRefresh, torrent.id]);

  useEffect(() => {
    return customSetInterval(async () => {
      const errorInterval = 10000;
      const liveInterval = 1000;
      const nonLiveInterval = 10000;

      return API.getTorrentStats(torrent.id)
        .then((stats) => {
          setStatsResponse(stats);
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
  }, [forceStatsRefresh, torrent.id]);

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

  const allSelected = !!(torrents && torrents.length > 0 && torrents.every((t) => selectedTorrentIds.has(t.id)));
  const someSelected = !!(torrents && torrents.some((t) => selectedTorrentIds.has(t.id)));

  const handleHeaderCheckbox = () => {
    if (allSelected) {
      clearSelection();
    } else if (torrents) {
      selectAll(torrents.map((t) => t.id));
    }
  };

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
          <th className="px-2 py-3 text-left font-medium text-gray-700 dark:text-slate-300">
            Name
          </th>
          <th className="w-24 px-2 py-3 text-center font-medium text-gray-700 dark:text-slate-300">
            Progress
          </th>
          <th className="w-24 px-2 py-3 text-right font-medium text-gray-700 dark:text-slate-300">
            Down
          </th>
          <th className="w-24 px-2 py-3 text-right font-medium text-gray-700 dark:text-slate-300">
            Up
          </th>
          <th className="w-20 px-2 py-3 text-center font-medium text-gray-700 dark:text-slate-300">
            ETA
          </th>
          <th className="w-16 px-2 py-3 text-center font-medium text-gray-700 dark:text-slate-300">
            Peers
          </th>
        </tr>
      </thead>
      <tbody>
        {torrents.map((torrent) => (
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
