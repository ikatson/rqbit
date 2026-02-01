import { TorrentListItem, STATE_INITIALIZING } from "../../api-types";
import { StatusIcon } from "../StatusIcon";
import { formatBytes } from "../../helper/formatBytes";
import { getCompletionETA } from "../../helper/getCompletionETA";
import { generateMagnetLink } from "../../helper/magnetUtils";
import { memo, useContext } from "react";
import { APIContext } from "../../context";
import { useErrorStore } from "../../stores/errorStore";
import { FaMagnet } from "react-icons/fa";
import { IconButton } from "../buttons/IconButton";

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
  const id = torrent.id;

  const API = useContext(APIContext);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const handleRowClick = (e: React.MouseEvent) => {
    onRowClick(torrent.id, e);
  };

  const handleCheckboxClick = (e: React.MouseEvent) => {
    e.stopPropagation();
    onCheckboxChange(torrent.id);
  };

  const copyMagnetLink = async () => {
    let trackers: string[] = [];
    try {
      const details = await API.getTorrentDetails(id);
      if (details.trackers) {
        trackers = details.trackers;
      }
    } catch (e) {
      console.warn("Could not fetch details for magnet, using basic link", e);
    }

    // Use shared helper for consistent formatting (uppercase hash, lowercase hex for trackers)
    const magnet = generateMagnetLink(
      torrent.info_hash,
      name || "",
      trackers
    );

    try {
      if (navigator.clipboard) {
        await navigator.clipboard.writeText(magnet);
      } else {
        const textArea = document.createElement("textarea");
        textArea.value = magnet;
        document.body.appendChild(textArea);
        textArea.select();
        document.execCommand("copy");
        document.body.removeChild(textArea);
      }
    } catch (e) {
      console.error("Failed to copy magnet link", e);
      setCloseableError({
        text: "Failed to copy magnet link",
        details: e as any,
      });
    }
  };

  // Common cell styles to avoid repetition
  const cellBase = "px-2 align-middle";
  const numericCell = `w-20 ${cellBase} text-right text-secondary whitespace-nowrap`;
  const centeredCell = `w-20 ${cellBase} text-center text-secondary whitespace-nowrap`;

  // Use table-fixed layout to match header column widths
  return (
    <table className="w-full table-fixed">
      <tbody>
        <tr
          onMouseDown={handleRowClick}
          className={`group cursor-pointer border-b border-divider text-sm h-8 ${
            isSelected ? "bg-primary/10" : "hover:bg-surface-raised"
          }`}
        >
          <td
            className={`w-8 ${cellBase} text-center`}
            onMouseDown={handleCheckboxClick}
          >
            <input
              type="checkbox"
              checked={isSelected}
              onChange={() => {}}
              className="w-4 h-4 rounded border-divider-strong bg-surface text-primary focus:ring-primary"
            />
          </td>
          <td className="w-8 px-1 align-middle">
            <StatusIcon
              className="w-5 h-5"
              error={!!error}
              live={live}
              finished={finished}
            />
          </td>
          <td
            className={`w-12 ${cellBase} text-center text-tertiary font-mono whitespace-nowrap`}
          >
            {torrent.id}
          </td>
          <td className={cellBase}>
            <div className="flex items-center justify-between gap-2">
                <div className="truncate" title={name}>
                {name || "Loading..."}
                </div>
                <div className="opacity-0 group-hover:opacity-100 transition-opacity">
                    <IconButton onClick={copyMagnetLink} title="Copy Magnet Link">
                        <FaMagnet className="hover:text-blue-500 w-3 h-3" />
                    </IconButton>
                </div>
            </div>
            {error && (
              <div className="truncate text-sm text-error" title={error}>
                {error}
              </div>
            )}
          </td>
          <td className={numericCell}>{formatBytes(totalBytes)}</td>
          <td className={`w-24 ${cellBase} text-center`}>
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
          <td className={numericCell}>{formatBytes(progressBytes)}</td>
          <td className={numericCell}>{downloadSpeed}</td>
          <td className={numericCell}>{uploadSpeed}</td>
          <td className={numericCell}>
            {uploadedBytes > 0 && <>{formatBytes(uploadedBytes)}</>}
          </td>
          <td className={centeredCell}>{displayEta}</td>
          <td
            className={`w-16 ${cellBase} text-center text-secondary whitespace-nowrap`}
          >
            {peersDisplay}
          </td>
        </tr>
      </tbody>
    </table>
  );
};

export const TorrentTableRow = memo(TorrentTableRowUnmemoized);
