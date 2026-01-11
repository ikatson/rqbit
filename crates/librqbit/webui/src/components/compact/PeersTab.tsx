import { useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import { PeerStats, PeerStatsSnapshot, TorrentStats } from "../../api-types";
import { APIContext } from "../../context";
import { formatBytes } from "../../helper/formatBytes";
import { SortIcon } from "../SortIcon";

interface PeersTabProps {
  torrentId: number;
  statsResponse: TorrentStats | null;
}

interface PeerWithSpeed {
  addr: string;
  stats: PeerStats;
  downloadSpeed: number;
  uploadSpeed: number;
}

type PeerSortColumn = "addr" | "connKind" | "downloadSpeed" | "uploadSpeed" | "downloaded" | "uploaded";
type PeerSortDirection = "asc" | "desc";

interface StatBadgeProps {
  label: string;
  value: number;
  color: string;
}

const StatBadge: React.FC<StatBadgeProps> = ({ label, value, color }) => (
  <span className="inline-flex items-center gap-1 text-xs">
    <span className={`font-semibold ${color}`}>{value}</span>
    <span className="text-gray-500 dark:text-slate-500">{label}</span>
  </span>
);

const formatSpeed = (bytesPerSecond: number): string => {
  if (bytesPerSecond === 0) return "-";
  return formatBytes(bytesPerSecond) + "/s";
};

export const PeersTab: React.FC<PeersTabProps> = ({ torrentId, statsResponse }) => {
  const API = useContext(APIContext);
  const [peerSnapshot, setPeerSnapshot] = useState<PeerStatsSnapshot | null>(null);
  const [peersWithSpeed, setPeersWithSpeed] = useState<PeerWithSpeed[]>([]);
  const [sortColumn, setSortColumn] = useState<PeerSortColumn>("downloadSpeed");
  const [sortDirection, setSortDirection] = useState<PeerSortDirection>("desc");

  const prevSnapshotRef = useRef<{ snapshot: PeerStatsSnapshot; timestamp: number } | null>(null);

  // Fetch peer stats periodically
  useEffect(() => {
    if (!statsResponse?.live) return;

    let cancelled = false;

    const fetchPeerStats = async () => {
      try {
        const snapshot = await API.getPeerStats(torrentId);
        if (!cancelled) {
          setPeerSnapshot(snapshot);
        }
      } catch (e) {
        // Ignore errors silently
      }
    };

    fetchPeerStats();
    const interval = setInterval(fetchPeerStats, 1000);

    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [API, torrentId, statsResponse?.live]);

  // Compute speeds when snapshot changes
  useEffect(() => {
    if (!peerSnapshot) {
      setPeersWithSpeed([]);
      return;
    }

    const now = Date.now();
    const prev = prevSnapshotRef.current;

    const newPeers: PeerWithSpeed[] = Object.entries(peerSnapshot.peers).map(([addr, stats]) => {
      let downloadSpeed = 0;
      let uploadSpeed = 0;

      if (prev && prev.snapshot.peers[addr]) {
        const timeDiffSec = (now - prev.timestamp) / 1000;
        if (timeDiffSec > 0) {
          const prevStats = prev.snapshot.peers[addr];
          const downloadedDiff = stats.counters.fetched_bytes - prevStats.counters.fetched_bytes;
          const uploadedDiff = stats.counters.uploaded_bytes - prevStats.counters.uploaded_bytes;
          downloadSpeed = Math.max(0, downloadedDiff / timeDiffSec);
          uploadSpeed = Math.max(0, uploadedDiff / timeDiffSec);
        }
      }

      return { addr, stats, downloadSpeed, uploadSpeed };
    });

    setPeersWithSpeed(newPeers);
    prevSnapshotRef.current = { snapshot: peerSnapshot, timestamp: now };
  }, [peerSnapshot]);

  const handleSort = useCallback((column: PeerSortColumn) => {
    setSortColumn((prevColumn) => {
      if (prevColumn === column) {
        setSortDirection((d) => (d === "asc" ? "desc" : "asc"));
        return column;
      }
      setSortDirection("desc");
      return column;
    });
  }, []);

  const sortedPeers = useMemo(() => {
    return [...peersWithSpeed].sort((a, b) => {
      let cmp = 0;
      switch (sortColumn) {
        case "addr":
          cmp = a.addr.localeCompare(b.addr);
          break;
        case "connKind":
          cmp = (a.stats.conn_kind ?? "").localeCompare(b.stats.conn_kind ?? "");
          break;
        case "downloadSpeed":
          cmp = a.downloadSpeed - b.downloadSpeed;
          break;
        case "uploadSpeed":
          cmp = a.uploadSpeed - b.uploadSpeed;
          break;
        case "downloaded":
          cmp = a.stats.counters.fetched_bytes - b.stats.counters.fetched_bytes;
          break;
        case "uploaded":
          cmp = a.stats.counters.uploaded_bytes - b.stats.counters.uploaded_bytes;
          break;
      }
      return sortDirection === "asc" ? cmp : -cmp;
    });
  }, [peersWithSpeed, sortColumn, sortDirection]);

  const peerStats = statsResponse?.live?.snapshot.peer_stats;

  if (!statsResponse) {
    return (
      <div className="p-4 text-gray-400 dark:text-slate-500">
        Loading...
      </div>
    );
  }

  if (!statsResponse.live) {
    return (
      <div className="p-4 text-gray-400 dark:text-slate-500">
        No peer information available (torrent may be paused)
      </div>
    );
  }

  const headerClass =
    "px-3 py-2 text-left text-xs font-medium text-gray-500 dark:text-slate-400 uppercase tracking-wider cursor-pointer hover:bg-gray-100 dark:hover:bg-slate-700 select-none";

  return (
    <div className="p-4">
      {/* Aggregate Stats - Compact inline display */}
      {peerStats && (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 mb-3 pb-2 border-b border-gray-200 dark:border-slate-700">
          <StatBadge label="connected" value={peerStats.live} color="text-green-600 dark:text-green-400" />
          <StatBadge label="connecting" value={peerStats.connecting} color="text-blue-600 dark:text-blue-400" />
          <StatBadge label="queued" value={peerStats.queued} color="text-yellow-600 dark:text-yellow-400" />
          <StatBadge label="seen" value={peerStats.seen} color="text-gray-600 dark:text-slate-300" />
          <StatBadge label="dead" value={peerStats.dead} color="text-red-600 dark:text-red-400" />
          <StatBadge label="not needed" value={peerStats.not_needed} color="text-gray-400 dark:text-slate-500" />
        </div>
      )}

      {/* Peer List Table */}
      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200 dark:divide-slate-700">
          <thead className="bg-gray-50 dark:bg-slate-800">
            <tr>
              <th className={headerClass} onClick={() => handleSort("addr")}>
                IP Address
                <SortIcon column="addr" sortColumn={sortColumn} sortDirection={sortDirection} />
              </th>
              <th className={headerClass} onClick={() => handleSort("connKind")}>
                Type
                <SortIcon column="connKind" sortColumn={sortColumn} sortDirection={sortDirection} />
              </th>
              <th className={headerClass} onClick={() => handleSort("downloadSpeed")}>
                Down Speed
                <SortIcon column="downloadSpeed" sortColumn={sortColumn} sortDirection={sortDirection} />
              </th>
              <th className={headerClass} onClick={() => handleSort("uploadSpeed")}>
                Up Speed
                <SortIcon column="uploadSpeed" sortColumn={sortColumn} sortDirection={sortDirection} />
              </th>
              <th className={headerClass} onClick={() => handleSort("downloaded")}>
                Downloaded
                <SortIcon column="downloaded" sortColumn={sortColumn} sortDirection={sortDirection} />
              </th>
              <th className={headerClass} onClick={() => handleSort("uploaded")}>
                Uploaded
                <SortIcon column="uploaded" sortColumn={sortColumn} sortDirection={sortDirection} />
              </th>
            </tr>
          </thead>
          <tbody className="bg-white dark:bg-slate-900 divide-y divide-gray-200 dark:divide-slate-700">
            {sortedPeers.length === 0 ? (
              <tr>
                <td
                  colSpan={6}
                  className="px-3 py-4 text-center text-gray-400 dark:text-slate-500"
                >
                  {peerSnapshot === null ? "Loading peer list..." : "No connected peers"}
                </td>
              </tr>
            ) : (
              sortedPeers.map((peer) => (
                <tr
                  key={peer.addr}
                  className="hover:bg-gray-50 dark:hover:bg-slate-800"
                >
                  <td className="px-3 py-2 text-sm font-mono text-gray-900 dark:text-slate-200">
                    {peer.addr}
                  </td>
                  <td className="px-3 py-2 text-sm text-gray-600 dark:text-slate-400 uppercase">
                    {peer.stats.conn_kind ?? "-"}
                  </td>
                  <td className="px-3 py-2 text-sm text-green-600 dark:text-green-400">
                    {formatSpeed(peer.downloadSpeed)}
                  </td>
                  <td className="px-3 py-2 text-sm text-blue-600 dark:text-blue-400">
                    {formatSpeed(peer.uploadSpeed)}
                  </td>
                  <td className="px-3 py-2 text-sm text-gray-600 dark:text-slate-400">
                    {formatBytes(peer.stats.counters.fetched_bytes)}
                  </td>
                  <td className="px-3 py-2 text-sm text-gray-600 dark:text-slate-400">
                    {formatBytes(peer.stats.counters.uploaded_bytes)}
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
};
