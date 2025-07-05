import { useContext, useEffect, useState, useMemo } from "react";
import {
  TorrentDetails,
  TorrentStats,
  PeerStatsSnapshot,
  TorrentIdWithStats,
} from "../api-types";
import { APIContext } from "../context";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { customSetInterval } from "../helper/customSetInterval";
import { FileListInput } from "./FileListInput";
import { formatBytes } from "../helper/formatBytes";
import { TorrentActions } from "./buttons/TorrentActions";

export const TorrentDetailsPane: React.FC<{
  torrent: TorrentIdWithStats;
  details: TorrentDetails | null;
}> = ({ details, torrent }) => {
  const [peers, setPeers] = useState<PeerStatsSnapshot | null>(null);
  const [activeTab, setActiveTab] = useState<"files" | "peers">("files");
  const [showAllPeers, setShowAllPeers] = useState(false);
  const [peerDownloadSpeeds, setPeerDownloadSpeeds] = useState<
    Record<string, number>
  >({});
  const [peerDownloadHistory, setPeerDownloadHistory] = useState<
    Record<string, Array<{ timestamp: number; fetched_bytes: number }>>
  >({});
  const [sortColumn, setSortColumn] = useState<string>("downloaded");
  const [sortDirection, setSortDirection] = useState<"asc" | "desc">("desc");
  const [extendedView, setExtendedView] = useState(false);

  const API = useContext(APIContext);

  useEffect(() => {
    const SPEED_WINDOW_MS = 10 * 1000; // 10 seconds

    if (activeTab !== "peers") {
      return;
    }

    return customSetInterval(async () => {
      const newPeers = await API.getTorrentPeerStats(
        torrent.id,
        showAllPeers ? "all" : "live",
      );
      const now = Date.now();

      const currentDownloadSpeeds: Record<string, number> = {};

      setPeerDownloadHistory((prevHistory) => {
        const newHistory: typeof prevHistory = {};
        Object.entries(newPeers.peers).forEach(([addr, peerStats]) => {
          newHistory[addr] = prevHistory[addr] ?? [];
          newHistory[addr].push({
            timestamp: now,
            fetched_bytes: peerStats.counters.fetched_bytes,
          });

          // Clean up old history entries
          newHistory[addr] = newHistory[addr].filter(
            (entry) => now - entry.timestamp <= SPEED_WINDOW_MS,
          );

          // Calculate speed using sliding window
          if (newHistory[addr].length > 1) {
            const firstEntry = newHistory[addr][0];
            const lastEntry = newHistory[addr][newHistory[addr].length - 1];
            const timeDiff =
              (lastEntry.timestamp - firstEntry.timestamp) / 1000; // in seconds
            const downloadedDiff =
              lastEntry.fetched_bytes - firstEntry.fetched_bytes;
            currentDownloadSpeeds[addr] =
              timeDiff > 0 ? downloadedDiff / timeDiff : 0; // bytes per second
          } else {
            currentDownloadSpeeds[addr] = 0;
          }
        });
        return newHistory;
      });

      setPeers(newPeers);
      setPeerDownloadSpeeds(currentDownloadSpeeds);

      return 1000; // Refresh every second while open
    }, 0);
  }, [torrent.id, torrent.stats.state, showAllPeers, activeTab]);

  const sortedPeers = useMemo(() => {
    if (!peers) return [];

    let peersArray = Object.entries(peers.peers);

    // Filter out "not_needed" peers if not showing all
    if (!showAllPeers) {
      peersArray = peersArray.filter(
        ([, peerStats]) => peerStats.state !== "not_needed",
      );
    }

    peersArray.sort(([addrA, peerStatsA], [addrB, peerStatsB]) => {
      let compareValue = 0;
      switch (sortColumn) {
        case "address":
          compareValue = addrA.localeCompare(addrB);
          break;
        case "state":
          compareValue = peerStatsA.state.localeCompare(peerStatsB.state);
          break;
        case "conn_kind":
          compareValue = (peerStatsA.conn_kind || "").localeCompare(
            peerStatsB.conn_kind || "",
          );
          break;
        case "downloaded":
          compareValue =
            peerStatsA.counters.fetched_bytes -
            peerStatsB.counters.fetched_bytes;
          break;
        case "down_speed":
          compareValue =
            (peerDownloadSpeeds[addrA] || 0) - (peerDownloadSpeeds[addrB] || 0);
          break;
        default:
          break;
      }
      return sortDirection === "asc" ? compareValue : -compareValue;
    });
    return peersArray;
  }, [peers, showAllPeers, sortColumn, sortDirection, peerDownloadSpeeds]);

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
    <div>
      <div className="mb-2 p-2 bg-gray-100 dark:bg-gray-800 rounded-md text-xs">
        <p className="font-bold text-sm">{torrent.name}</p>
        <p>ID: {torrent.id}</p>
        <p>Size: {formatBytes(torrent.stats.total_bytes)}</p>
        <p>Info Hash: {torrent.info_hash}</p>
        <p>Output folder: {torrent.output_folder}</p>
      </div>
      <div className="mt-2">
        <TorrentActions
          id={torrent.id}
          statsResponse={torrent.stats}
          detailsResponse={details}
          extendedView={extendedView}
          setExtendedView={setExtendedView}
        />
      </div>
      <div className="flex border-b">
        <button
          className={`px-4 py-2 text-sm ${activeTab === "files" ? "border-b-2 border-blue-500" : ""}`}
          onClick={() => setActiveTab("files")}
        >
          Files
        </button>
        <button
          className={`px-4 py-2 text-sm ${activeTab === "peers" ? "border-b-2 border-blue-500" : ""}`}
          onClick={() => setActiveTab("peers")}
        >
          Peers
        </button>
      </div>
      {activeTab === "peers" && (
        <div className="flex items-center mt-2 text-sm">
          <input
            type="checkbox"
            id="showAllPeers"
            checked={showAllPeers}
            onChange={(e) => setShowAllPeers(e.target.checked)}
            className="mr-2"
          />
          <label htmlFor="showAllPeers">Show all peers</label>
        </div>
      )}
      <div className="p-4 text-xs">
        {activeTab === "files" && details && (
          <FileListInput
            torrentId={torrent.id}
            torrentDetails={details}
            torrentStats={torrent.stats}
            selectedFiles={new Set(details.files.map((_, i) => i))}
            setSelectedFiles={() => {}}
            disabled={false}
            allowStream={false}
            showProgressBar={true}
          />
        )}
        {activeTab === "peers" && peers && (
          <div className="overflow-x-auto">
            <table className="min-w-full divide-y divide-gray-200 dark:divide-gray-700">
              <thead className="bg-gray-50 dark:bg-gray-800">
                <tr>
                  <th
                    className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer"
                    onClick={() => handleSort("address")}
                  >
                    Address{getSortIndicator("address")}
                  </th>
                  <th
                    className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer"
                    onClick={() => handleSort("state")}
                  >
                    State{getSortIndicator("state")}
                  </th>
                  <th
                    className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer"
                    onClick={() => handleSort("conn_kind")}
                  >
                    Conn. Kind{getSortIndicator("conn_kind")}
                  </th>
                  <th
                    className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer"
                    onClick={() => handleSort("downloaded")}
                  >
                    Downloaded{getSortIndicator("downloaded")}
                  </th>
                  <th
                    className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer"
                    onClick={() => handleSort("down_speed")}
                  >
                    Down Speed{getSortIndicator("down_speed")}
                  </th>
                </tr>
              </thead>
              <tbody className="bg-white divide-y divide-gray-200 dark:bg-gray-900 dark:divide-gray-700">
                {sortedPeers.map(([addr, peerStats]) => (
                  <tr key={addr}>
                    <td className="px-2 py-1 whitespace-nowrap">{addr}</td>
                    <td className="px-2 py-1 whitespace-nowrap">
                      {peerStats.state}
                    </td>
                    <td className="px-2 py-1 whitespace-nowrap">
                      {peerStats.conn_kind || "N/A"}
                    </td>
                    <td className="px-2 py-1 whitespace-nowrap">
                      {formatBytes(peerStats.counters.fetched_bytes)}
                    </td>
                    <td className="px-2 py-1 whitespace-nowrap">
                      {formatBytes(peerDownloadSpeeds[addr] || 0)}/s
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
};
