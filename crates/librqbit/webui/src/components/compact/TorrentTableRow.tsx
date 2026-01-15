import { TorrentListItem, STATE_INITIALIZING } from "../../api-types";
import { StatusIcon } from "../StatusIcon";
import { formatBytes } from "../../helper/formatBytes";
import { getCompletionETA } from "../../helper/getCompletionETA";
import { memo } from "react";

interface TorrentTableRowProps {
  torrent: TorrentListItem;
  isSelected: boolean;
  onRowClick: (id: number, e: React.MouseEvent) => void;
  onCheckboxChange: (id: number) => void;
}

const TorrentTableRowUnmemoized: React.FC<TorrentTableRowProps> = ({
  torrent,
  isSelected,
  onRowClick,
  onCheckboxChange,
}) => {
  const stats = torrent.stats;
  const state = stats?.state ?? "";
  const error = stats?.error ?? null;
  const totalBytes = stats?.total_bytes ?? 1;
  const progressBytes = stats?.progress_bytes ?? 0;
  const finished = stats?.finished || false;
  const live = !!stats?.live;

  const progressPercentage = error
    ? 100
    : totalBytes === 0
      ? 100
      : Math.round((progressBytes / totalBytes) * 100);

  const downloadSpeed = stats?.live?.download_speed?.human_readable ?? "-";
  const uploadSpeed = stats?.live?.upload_speed?.human_readable ?? "-";
  const uploadedBytes = stats?.live?.snapshot.uploaded_bytes ?? 0;

  const peerStats = stats?.live?.snapshot.peer_stats;
  const peersDisplay = peerStats ? `${peerStats.live}/${peerStats.seen}` : "-";

  const eta = stats ? getCompletionETA(stats) : "-";
  const displayEta = finished ? "Done" : eta;

  const name = torrent.name ?? "";

  const handleRowClick = (e: React.MouseEvent) => {
    onRowClick(torrent.id, e);
  };

  const handleCheckboxClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    onCheckboxChange(torrent.id);
  };

  return (
    <tr
      onMouseDown={handleRowClick}
      className={`cursor-pointer border-b border-divider transition-colors ${
        isSelected ? "bg-primary/10" : "hover:bg-surface-raised"
      }`}
    >
      <td className="px-2 py-2 text-center" onMouseDown={handleCheckboxClick}>
        <input
          type="checkbox"
          checked={isSelected}
          onChange={() => {}}
          className="w-4 h-4 rounded border-divider-strong bg-surface text-primary focus:ring-primary"
        />
      </td>
      <td className="px-1 py-2">
        <StatusIcon
          className="w-5 h-5"
          error={!!error}
          live={live}
          finished={finished}
        />
      </td>
      <td className="px-2 py-2 text-center text-tertiary font-mono">
        {torrent.id}
      </td>
      <td className="px-2 py-2 max-w-xs">
        <div className="truncate" title={name}>
          {name || "Loading..."}
        </div>
        {error && (
          <div className="truncate text-sm text-error" title={error}>
            {error}
          </div>
        )}
      </td>
      <td className="px-2 py-2 text-right text-secondary">
        {formatBytes(totalBytes)}
      </td>
      <td className="px-2 py-2 text-center">
        <div className="flex items-center gap-2">
          <div className="flex-1 h-1.5 bg-divider rounded-full overflow-hidden">
            <div
              className={`h-full rounded-full ${
                error
                  ? "bg-error-bg"
                  : finished
                    ? "bg-success-bg"
                    : state === STATE_INITIALIZING
                      ? "bg-warning-bg"
                      : "bg-primary-bg"
              }`}
              style={{ width: `${progressPercentage}%` }}
            />
          </div>
          <span className="text-sm text-secondary w-8 text-right">
            {progressPercentage}%
          </span>
        </div>
      </td>
      <td className="px-2 py-2 text-right text-secondary whitespace-nowrap">
        {formatBytes(progressBytes)}
      </td>
      <td className="px-2 py-2 text-right text-secondary whitespace-nowrap">
        {downloadSpeed}
      </td>
      <td className="px-2 py-2 text-right text-secondary whitespace-nowrap">
        {uploadSpeed}
      </td>
      <td className="px-2 py-2 text-right text-secondary whitespace-nowrap">
        {uploadedBytes > 0 && <>{formatBytes(uploadedBytes)}</>}
      </td>
      <td className="px-2 py-2 text-center text-secondary">{displayEta}</td>
      <td className="px-2 py-2 text-center text-secondary">{peersDisplay}</td>
    </tr>
  );
};

export const TorrentTableRow = memo(TorrentTableRowUnmemoized);
