import { TorrentDetails, TorrentStats, STATE_INITIALIZING } from "../../api-types";
import { StatusIcon } from "../StatusIcon";
import { formatBytes } from "../../helper/formatBytes";
import { torrentDisplayName } from "../../helper/getTorrentDisplayName";
import { getCompletionETA } from "../../helper/getCompletionETA";

interface TorrentTableRowProps {
  id: number;
  detailsResponse: TorrentDetails | null;
  statsResponse: TorrentStats | null;
  isSelected: boolean;
  onRowClick: () => void;
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

  const downloadSpeed = statsResponse?.live?.download_speed?.human_readable ?? "-";
  const uploadSpeed = statsResponse?.live?.upload_speed?.human_readable ?? "-";

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
      onClick={onRowClick}
      className={`
        cursor-pointer border-b border-gray-100 dark:border-slate-700
        transition-colors
        ${isSelected
          ? "bg-blue-50 dark:bg-slate-700"
          : "hover:bg-gray-50 dark:hover:bg-slate-800"
        }
      `}
    >
      <td className="w-8 px-2 py-2 text-center" onClick={handleCheckboxClick}>
        <input
          type="checkbox"
          checked={isSelected}
          onChange={() => {}}
          className="w-4 h-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500 dark:border-slate-600 dark:bg-slate-800"
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
      <td className="px-2 py-2 max-w-xs">
        <div className="truncate text-sm font-medium text-gray-900 dark:text-slate-200" title={name}>
          {name || "Loading..."}
        </div>
        {error && (
          <div className="truncate text-xs text-red-500" title={error}>
            {error}
          </div>
        )}
      </td>
      <td className="w-24 px-2 py-2 text-center">
        <div className="flex items-center gap-2">
          <div className="flex-1 h-1.5 bg-gray-200 dark:bg-slate-600 rounded-full overflow-hidden">
            <div
              className={`h-full rounded-full ${
                error
                  ? "bg-red-500"
                  : finished
                    ? "bg-green-500"
                    : state === STATE_INITIALIZING
                      ? "bg-yellow-500"
                      : "bg-blue-500"
              }`}
              style={{ width: `${progressPercentage}%` }}
            />
          </div>
          <span className="text-xs text-gray-600 dark:text-slate-400 w-8 text-right">
            {progressPercentage}%
          </span>
        </div>
      </td>
      <td className="w-24 px-2 py-2 text-right text-sm text-gray-600 dark:text-slate-400">
        {downloadSpeed}
      </td>
      <td className="w-24 px-2 py-2 text-right text-sm text-gray-600 dark:text-slate-400">
        {uploadSpeed}
      </td>
      <td className="w-20 px-2 py-2 text-center text-sm text-gray-600 dark:text-slate-400">
        {displayEta}
      </td>
      <td className="w-16 px-2 py-2 text-center text-sm text-gray-600 dark:text-slate-400">
        {peersDisplay}
      </td>
    </tr>
  );
};
