
import { TorrentIdWithStats, TorrentDetails } from "../api-types";
import { Spinner } from "./Spinner";
import { Torrent } from "./Torrent";
import { useContext, useEffect, useState, useMemo } from "react";
import { APIContext } from "../context";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { torrentDisplayName } from "../helper/getTorrentDisplayName";
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { Speed } from "./Speed";

export const CompactTorrentsList = (props: {
  torrents: Array<TorrentIdWithStats> | null;
  loading: boolean;
  onTorrentClick: (id: number) => void;
  selectedTorrent: number | null;
}) => {
  const [allTorrentDetails, setAllTorrentDetails] = useState<Array<{ id: number, details: TorrentDetails | null }>>([]);
  const [sortColumn, setSortColumn] = useState<string>("name");
  const [sortDirection, setSortDirection] = useState<"asc" | "desc">("asc");

  const API = useContext(APIContext);

  useEffect(() => {
    if (!props.torrents) {
      setAllTorrentDetails([]);
      return;
    }

    const fetchAllTorrentDetails = async () => {
      const data = await Promise.all(props.torrents!.map(async (t) => {
        const details = await API.getTorrentDetails(t.id);
        return { id: t.id, details };
      }));
      setAllTorrentDetails(data);
    };

    return loopUntilSuccess(fetchAllTorrentDetails, 5000);
  }, [props.torrents]);

  const sortedTorrentData = useMemo(() => {
    if (!props.torrents) return [];

    const combinedData = props.torrents.map(torrentWithStats => {
      const details = allTorrentDetails.find(d => d.id === torrentWithStats.id)?.details || null;
      return { ...torrentWithStats, details };
    });

    const sortableData = [...combinedData];

    sortableData.sort((a, b) => {
      let compareValue = 0;
      switch (sortColumn) {
        case "id":
          compareValue = a.id - b.id;
          break;
        case "name":
          compareValue = (torrentDisplayName(a.details) || "").localeCompare(torrentDisplayName(b.details) || "");
          break;
        case "progress":
          const progressA = a.stats?.progress_bytes || 0;
          const totalA = a.stats?.total_bytes || 1;
          const progressB = b.stats?.progress_bytes || 0;
          const totalB = b.stats?.total_bytes || 1;
          compareValue = (progressA / totalA) - (progressB / totalB);
          break;
        case "speed":
          compareValue = (a.stats?.live?.download_speed.mbps || 0) - (b.stats?.live?.download_speed.mbps || 0);
          break;
        case "eta":
          const etaA = a.stats?.live?.time_remaining?.duration?.secs || Infinity;
          const etaB = b.stats?.live?.time_remaining?.duration?.secs || Infinity;
          compareValue = etaA - etaB;
          break;
        case "peers":
          compareValue = (a.stats?.live?.snapshot.peer_stats.live || 0) - (b.stats?.live?.snapshot.peer_stats.live || 0);
          break;
        case "size":
          compareValue = (a.stats?.total_bytes || 0) - (b.stats?.total_bytes || 0);
          break;
        default:
          break;
      }
      return sortDirection === "asc" ? compareValue : -compareValue;
    });
    return sortableData;
  }, [props.torrents, allTorrentDetails, sortColumn, sortDirection]);

  const handleSort = (column: string) => {
    if (sortColumn === column) {
      setSortDirection(sortDirection === "asc" ? "desc" : "asc");
    } else {
      setSortColumn(column);
      setSortDirection("asc"); // Default to ascending when changing column
    }
  };

  const getSortIndicator = (column: string) => {
    if (sortColumn === column) {
      return sortDirection === "asc" ? " 🔼" : " 🔽";
    }
    return "";
  };

  return (
    <div className="flex flex-col gap-2 mx-2 pb-3 sm:px-7">
      {props.torrents === null ? (
        props.loading ? (
          <Spinner
            className="justify-center m-5"
            label="Loading torrent list"
          />
        ) : null
      ) : props.torrents.length === 0 ? (
        <p className="text-center">No existing torrents found.</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="min-w-full divide-y divide-gray-200 dark:divide-gray-700">
            <thead className="bg-gray-50 dark:bg-gray-800">
              <tr>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer" onClick={() => handleSort("id")}>ID{getSortIndicator("id")}</th>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider"></th>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer" onClick={() => handleSort("name")}>Name{getSortIndicator("name")}</th>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer" onClick={() => handleSort("progress")}>Progress{getSortIndicator("progress")}</th>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer" onClick={() => handleSort("speed")}>Speed{getSortIndicator("speed")}</th>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer" onClick={() => handleSort("eta")}>ETA{getSortIndicator("eta")}</th>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer" onClick={() => handleSort("peers")}>Peers{getSortIndicator("peers")}</th>
                <th className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer" onClick={() => handleSort("size")}>Size{getSortIndicator("size")}</th>
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200 dark:bg-gray-900 dark:divide-gray-700">
              {sortedTorrentData.map((t) => (
                <Torrent
                  key={t.id}
                  torrent={t}
                  compact
                  onClick={() => props.onTorrentClick(t.id)}
                  selected={t.id === props.selectedTorrent}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
};
