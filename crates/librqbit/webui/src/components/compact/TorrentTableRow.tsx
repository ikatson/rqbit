import {
  TorrentDetails,
  TorrentStats,
  STATE_INITIALIZING,
} from "../../api-types";
import { StatusIcon } from "../StatusIcon";
import { formatBytes } from "../../helper/formatBytes";
import { torrentDisplayName } from "../../helper/getTorrentDisplayName";
import { getCompletionETA } from "../../helper/getCompletionETA";

interface TorrentTableRowProps {
  id: number;
  detailsResponse: TorrentDetails | null;
  statsResponse: TorrentStats | null;
  isSelected: boolean;
  onRowClick: (e: React.MouseEvent) => void;
  onCheckboxChange: () => void;
}

export const TorrentTableRow: React.FC<TorrentTableRowProps> = ({
  id,
  detailsResponse,
  statsResponse,
  isSelected,
  onRowClick,
  onCheckboxChange,
}) => {
  const state = statsResponse?.state ?? "";
  const error = statsResponse?.error ?? null;
  const totalBytes = statsResponse?.total_bytes ?? 1;
  const progressBytes = statsResponse?.progress_bytes ?? 0;
  const finished = statsResponse?.finished || false;
  const live = !!statsResponse?.live;

  const progressPercentage = error
    ? 100
    : totalBytes === 0
      ? 100
      : Math.round((progressBytes / totalBytes) * 100);

  const downloadSpeed =
    statsResponse?.live?.download_speed?.human_readable ?? "-";
  const uploadSpeed = statsResponse?.live?.upload_speed?.human_readable ?? "-";
  const uploadedBytes = statsResponse?.live?.snapshot.uploaded_bytes ?? 0;

  const peerStats = statsResponse?.live?.snapshot.peer_stats;
  const peersDisplay = peerStats ? `${peerStats.live}/${peerStats.seen}` : "-";

  const eta = statsResponse ? getCompletionETA(statsResponse) : "-";
  const displayEta = finished ? "Done" : eta;

  const name = torrentDisplayName(detailsResponse);

  const handleCheckboxClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    onCheckboxChange();
  };

  return (
    <tr
      onMouseDown={(e) => onRowClick(e)}
      className={`cursor-pointer border-b border-border transition-colors ${
        isSelected ? "bg-primary/10" : "hover:bg-surface-raised"
      }`}
    >
      <td
        className="w-8 px-2 py-2 text-center"
        onMouseDown={handleCheckboxClick}
      >
        <input
          type="checkbox"
          checked={isSelected}
          onChange={() => {}}
          className="w-4 h-4 rounded border-border-strong bg-surface text-primary focus:ring-primary"
        />
      </td>
      <td className="w-8 px-1 py-2">
        <StatusIcon
          className="w-5 h-5"
          error={!!error}
          live={live}
          finished={finished}
        />
      </td>
      <td className="w-12 px-2 py-2 text-center text-text-tertiary font-mono">
        {id}
      </td>
      <td className="px-2 py-2 max-w-xs">
        <div className="truncate text-text" title={name}>
          {name || "Loading..."}
        </div>
        {error && (
          <div className="truncate text-sm text-error" title={error}>
            {error}
          </div>
        )}
      </td>
      <td className="w-20 px-2 py-2 text-right ext-text-secondary">
        {formatBytes(totalBytes)}
      </td>
      <td className="w-24 px-2 py-2 text-center">
        <div className="flex items-center gap-2">
          <div className="flex-1 h-1.5 bg-border rounded-full overflow-hidden">
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
          <span className="text-sm text-text-secondary w-8 text-right">
            {progressPercentage}%
          </span>
        </div>
      </td>
      <td className="w-24 px-2 py-2 text-right text-text-secondary">
        {downloadSpeed}
      </td>
      <td className="w-24 px-2 py-2 text-right text-text-secondary">
        {uploadSpeed}
      </td>
      <td className="w-24 px-2 py-2 text-right text-text-secondary">
        {uploadedBytes > 0 && <>{formatBytes(uploadedBytes)}</>}
      </td>
      <td className="w-20 px-2 py-2 text-center text-text-secondary">
        {displayEta}
      </td>
      <td className="w-16 px-2 py-2 text-center text-text-secondary">
        {peersDisplay}
      </td>
    </tr>
  );
};
