import { useContext, useEffect, useState, useMemo } from "react";
import { PeerStatsSnapshot, TorrentIdWithStats } from "../api-types";
import { APIContext } from "../context";
import { customSetInterval } from "../helper/customSetInterval";
import { formatBytes } from "../helper/formatBytes";
import { FaArrowUp, FaArrowDown } from "react-icons/fa";

export const PeerTable: React.FC<{
  torrent: TorrentIdWithStats;
}> = ({ torrent }) => {
  const [peers, setPeers] = useState<PeerStatsSnapshot | null>(null);
  const [peerSpeeds, setPeerSpeeds] = useState<
    Record<string, { download: number; upload: number }>
  >({});
  const [peerHistory, setPeerHistory] = useState<
    Record<
      string,
      Array<{
        timestamp: number;
        fetched_bytes: number;
        uploaded_bytes: number;
      }>
    >
  >({});
  const [sortColumn, setSortColumn] = useState<string>("downloaded");
  const [sortDirection, setSortDirection] = useState<"asc" | "desc">("desc");

  const API = useContext(APIContext);

  useEffect(() => {
    const SPEED_WINDOW_MS = 10 * 1000; // 10 seconds

    return customSetInterval(async () => {
      const newPeers = await API.getTorrentPeerStats(torrent.id, "live");
      const now = Date.now();

      const currentSpeeds: Record<
        string,
        { download: number; upload: number }
      > = {};

      setPeerHistory((prevHistory) => {
        const newHistory: typeof prevHistory = {};
        Object.entries(newPeers.peers).forEach(([addr, peerStats]) => {
          newHistory[addr] = prevHistory[addr] ?? [];
          newHistory[addr].push({
            timestamp: now,
            fetched_bytes: peerStats.counters.fetched_bytes,
            uploaded_bytes: peerStats.counters.uploaded_bytes,
          });

          // Clean up old history entries
          newHistory[addr] = newHistory[addr].filter(
            (entry) => now - entry.timestamp <= SPEED_WINDOW_MS,
          );

          // Calculate speeds using sliding window
          if (newHistory[addr].length > 1) {
            const firstEntry = newHistory[addr][0];
            const lastEntry = newHistory[addr][newHistory[addr].length - 1];
            const timeDiff =
              (lastEntry.timestamp - firstEntry.timestamp) / 1000; // in seconds

            if (timeDiff > 0) {
              // Calculate download speed
              const downloadedDiff =
                lastEntry.fetched_bytes - firstEntry.fetched_bytes;
              // Calculate upload speed
              const uploadedDiff =
                lastEntry.uploaded_bytes - firstEntry.uploaded_bytes;

              currentSpeeds[addr] = {
                download: downloadedDiff / timeDiff, // bytes per second
                upload: uploadedDiff / timeDiff, // bytes per second
              };
            } else {
              currentSpeeds[addr] = { download: 0, upload: 0 };
            }
          } else {
            currentSpeeds[addr] = { download: 0, upload: 0 };
          }
        });
        return newHistory;
      });

      setPeers(newPeers);
      setPeerSpeeds(currentSpeeds);

      return 1000; // Refresh every second while open
    }, 0);
  }, [torrent.id, torrent.stats.state]);

  const sortedPeers = useMemo(() => {
    if (!peers) return [];

    let peersArray = Object.entries(peers.peers);

    peersArray.sort(([addrA, peerStatsA], [addrB, peerStatsB]) => {
      let compareValue = 0;
      switch (sortColumn) {
        case "address":
          compareValue = addrA.localeCompare(addrB);
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
            (peerSpeeds[addrA]?.download || 0) -
            (peerSpeeds[addrB]?.download || 0);
          break;
        case "up_speed":
          compareValue =
            (peerSpeeds[addrA]?.upload || 0) - (peerSpeeds[addrB]?.upload || 0);
          break;
        default:
          break;
      }
      return sortDirection === "asc" ? compareValue : -compareValue;
    });
    return peersArray;
  }, [peers, sortColumn, sortDirection, peerSpeeds]);

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
      return sortDirection === "asc" ? (
        <FaArrowUp className="inline ml-1" />
      ) : (
        <FaArrowDown className="inline ml-1" />
      );
    }
    return "";
  };

  return (
    <>
      {peers && (
        <div className="overflow-x-auto text-xs">
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
                <th
                  className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer"
                  onClick={() => handleSort("downloaded")}
                >
                  Uploaded{getSortIndicator("uploaded")}
                </th>
                <th
                  className="px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer"
                  onClick={() => handleSort("up_speed")}
                >
                  Up Speed{getSortIndicator("up_speed")}
                </th>
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200 dark:bg-gray-900 dark:divide-gray-700">
              {sortedPeers.map(([addr, peerStats]) => (
                <tr key={addr}>
                  <td className="px-2 py-1 whitespace-nowrap">{addr}</td>
                  <td className="px-2 py-1 whitespace-nowrap">
                    {peerStats.conn_kind || "N/A"}
                  </td>
                  <td className="px-2 py-1 whitespace-nowrap">
                    {formatBytes(peerStats.counters.fetched_bytes)}
                  </td>
                  <td className="px-2 py-1 whitespace-nowrap">
                    {formatBytes(peerSpeeds[addr]?.download || 0)}/s
                  </td>
                  <td className="px-2 py-1 whitespace-nowrap">
                    {formatBytes(peerStats.counters.uploaded_bytes)}
                  </td>
                  <td className="px-2 py-1 whitespace-nowrap">
                    {formatBytes(peerSpeeds[addr]?.upload || 0)}/s
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
};
