import { TorrentStats } from "../../api-types";

interface PeersTabProps {
  torrentId: number;
  statsResponse: TorrentStats | null;
}

interface StatCardProps {
  label: string;
  value: number;
  color: string;
}

const StatCard: React.FC<StatCardProps> = ({ label, value, color }) => (
  <div className="bg-gray-50 dark:bg-slate-800 rounded-lg p-3">
    <div className={`text-2xl font-bold ${color}`}>{value}</div>
    <div className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">
      {label}
    </div>
  </div>
);

export const PeersTab: React.FC<PeersTabProps> = ({ torrentId, statsResponse }) => {
  const peerStats = statsResponse?.live?.snapshot.peer_stats;

  if (!statsResponse) {
    return (
      <div className="p-4 text-gray-400 dark:text-slate-500">
        Loading...
      </div>
    );
  }

  if (!peerStats) {
    return (
      <div className="p-4 text-gray-400 dark:text-slate-500">
        No peer information available (torrent may be paused)
      </div>
    );
  }

  return (
    <div className="p-4">
      <div className="grid grid-cols-3 lg:grid-cols-6 gap-3">
        <StatCard
          label="Connected"
          value={peerStats.live}
          color="text-green-600 dark:text-green-400"
        />
        <StatCard
          label="Connecting"
          value={peerStats.connecting}
          color="text-blue-600 dark:text-blue-400"
        />
        <StatCard
          label="Queued"
          value={peerStats.queued}
          color="text-yellow-600 dark:text-yellow-400"
        />
        <StatCard
          label="Seen"
          value={peerStats.seen}
          color="text-gray-600 dark:text-slate-300"
        />
        <StatCard
          label="Dead"
          value={peerStats.dead}
          color="text-red-600 dark:text-red-400"
        />
        <StatCard
          label="Not Needed"
          value={peerStats.not_needed}
          color="text-gray-400 dark:text-slate-500"
        />
      </div>
      <p className="mt-4 text-xs text-gray-400 dark:text-slate-500">
        Detailed peer list is not available in this version.
      </p>
    </div>
  );
};
