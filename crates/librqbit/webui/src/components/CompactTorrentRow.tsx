import { useMemo } from "react";
import { TorrentIdWithStats, STATE_INITIALIZING } from "../api-types";
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { useTorrentStore } from "../stores/torrentStore";
import { ProgressBar } from "./ProgressBar";
import { StatusIcon } from "./StatusIcon";

export const CompactTorrentRow: React.FC<{
  torrent: TorrentIdWithStats;
}> = ({ torrent }) => {
  const selected = useTorrentStore((state) =>
    state.selectedTorrentIds.includes(torrent.id),
  );
  const setSelectedTorrentId = useTorrentStore(
    (state) => state.setSelectedTorrentId,
  );
  const toggleSelectedTorrentId = useTorrentStore(
    (state) => state.toggleSelectedTorrentId,
  );
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

  const formatPeersString = () => {
    let peer_stats = statsResponse?.live?.snapshot.peer_stats;
    if (!peer_stats) {
      return "";
    }
    return `${peer_stats.live} / ${peer_stats.seen}`;
  };

  const onClick = useMemo(() => {
    return (event: React.MouseEvent<HTMLTableRowElement>) => {
      event.stopPropagation();
      event.preventDefault();
      if (event.metaKey || event.ctrlKey || event.shiftKey) {
        toggleSelectedTorrentId(torrent.id);
      } else {
        setSelectedTorrentId(torrent.id);
      }
    };
  }, [torrent.id]);

  return (
    <tr
      className={`cursor-pointer ${selected ? "bg-gray-200 dark:bg-slate-700" : ""}`}
      onClick={onClick}
    >
      <td className="px-2 py-1 whitespace-nowrap  text-xs">{torrent.id}</td>
      <td className="px-2 py-1 whitespace-nowrap">
        <StatusIcon
          className="w-4 h-4"
          error={!!error}
          live={!!statsResponse?.live}
          finished={finished}
        />
      </td>
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
        {statsResponse.live?.download_speed.human_readable}
      </td>
      <td className="px-2 py-1 whitespace-nowrap text-xs">
        {statsResponse.live?.upload_speed.human_readable}
      </td>
      <td className="px-2 py-1 whitespace-nowrap text-xs">
        {getCompletionETA(statsResponse)}
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
