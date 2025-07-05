import {
  TorrentDetails,
  TorrentStats,
  STATE_INITIALIZING,
  TorrentIdWithStats,
} from "../api-types";
import { torrentDisplayName } from "../helper/getTorrentDisplayName";
import { StatusIcon } from "./StatusIcon";
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { ProgressBar } from "./ProgressBar";
import { Speed } from "./Speed";

export const CompactTorrentRow: React.FC<{
  torrent: TorrentIdWithStats;
  onClick: () => void;
  selected: boolean;
}> = ({ torrent, onClick, selected }) => {
  const statsResponse = torrent.stats;
  const error = statsResponse?.error ?? null;
  const finished = statsResponse?.finished || false;
  const totalBytes = statsResponse?.total_bytes ?? 1;
  const progressBytes = statsResponse?.progress_bytes ?? 0;
  const progressPercentage = error
    ? 100
    : totalBytes == 0
      ? 100
      : (progressBytes / totalBytes) * 100;

  const statusIcon = (className: string) => {
    return (
      <StatusIcon
        className={className}
        error={!!error}
        live={!!statsResponse?.live}
        finished={finished}
      />
    );
  };

  const formatPeersString = () => {
    let peer_stats = statsResponse?.live?.snapshot.peer_stats;
    if (!peer_stats) {
      return "";
    }
    return `${peer_stats.live} / ${peer_stats.seen}`;
  };

  return (
    <tr
      className={`cursor-pointer ${selected ? "bg-gray-200 dark:bg-slate-700" : ""}`}
      onClick={onClick}
    >
      <td className="px-2 py-1 whitespace-nowrap text-xs">{torrent.id}</td>
      <td className="px-2 py-1 whitespace-nowrap">{statusIcon("w-4 h-4")}</td>
      <td className="px-2 py-1 text-left text-xs text-gray-900 text-ellipsis overflow-hidden dark:text-slate-200">
        {torrent.name}
      </td>
      <td className="px-2 py-1 whitespace-nowrap text-xs">
        {error ? (
          <span className="text-red-500 text-xs">Error</span>
        ) : (
          <ProgressBar
            now={progressPercentage}
            label={error}
            variant={
              statsResponse?.state == STATE_INITIALIZING
                ? "warn"
                : finished
                  ? "success"
                  : "info"
            }
          />
        )}
      </td>
      <td className="px-2 py-1 whitespace-nowrap text-xs">
        {statsResponse && <Speed statsResponse={statsResponse} />}
      </td>
      <td className="px-2 py-1 whitespace-nowrap text-xs">
        {statsResponse && getCompletionETA(statsResponse)}
      </td>
      <td className="px-2 py-1 whitespace-nowrap text-xs">
        {formatPeersString()}
      </td>
      <td className="px-2 py-1 whitespace-nowrap text-xs">
        {formatBytes(totalBytes)}
      </td>
    </tr>
  );
};
