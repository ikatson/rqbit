import { useState } from "react";
import {
  TorrentDetails,
  TorrentStats,
  STATE_INITIALIZING,
  STATE_LIVE,
  STATE_PAUSED,
  STATE_ERROR,
} from "../../api-types";
import { formatBytes } from "../../helper/formatBytes";
import { torrentDisplayName } from "../../helper/getTorrentDisplayName";
import { getCompletionETA } from "../../helper/getCompletionETA";
import { FaCopy, FaCheck } from "react-icons/fa";
import { PiecesCanvas } from "./PiecesCanvas";

interface OverviewTabProps {
  torrentId: number;
  detailsResponse: TorrentDetails | null;
  statsResponse: TorrentStats | null;
}

export const OverviewTab: React.FC<OverviewTabProps> = ({
  torrentId,
  detailsResponse,
  statsResponse,
}) => {
  const [copied, setCopied] = useState(false);

  if (!detailsResponse || !statsResponse) {
    return <div className="p-4 text-text-tertiary">Loading...</div>;
  }

  const name = torrentDisplayName(detailsResponse);
  const infoHash = detailsResponse.info_hash;
  const state = statsResponse.state;
  const error = statsResponse.error;
  const totalBytes = statsResponse.total_bytes ?? 1;
  const progressBytes = statsResponse.progress_bytes ?? 0;
  const finished = statsResponse.finished || false;
  const totalUploadedBytes = statsResponse.live?.snapshot.uploaded_bytes ?? 0;

  const progressPercentage = error
    ? 100
    : totalBytes === 0
      ? 100
      : (progressBytes / totalBytes) * 100;

  const downloadSpeed =
    statsResponse.live?.download_speed?.human_readable ?? "-";
  const uploadSpeed = statsResponse.live?.upload_speed?.human_readable ?? "-";
  const eta = getCompletionETA(statsResponse);

  const peerStats = statsResponse.live?.snapshot.peer_stats;
  const peersConnected = peerStats?.live ?? 0;
  const peersSeen = peerStats?.seen ?? 0;

  const getStateDisplay = () => {
    if (error) return { text: "Error", color: "text-error" };
    if (state === STATE_INITIALIZING)
      return { text: "Initializing", color: "text-warning" };
    if (state === STATE_PAUSED)
      return { text: "Paused", color: "text-text-secondary" };
    if (state === STATE_LIVE && finished)
      return { text: "Seeding", color: "text-success" };
    if (state === STATE_LIVE)
      return { text: "Downloading", color: "text-primary" };
    return { text: state, color: "text-text-secondary" };
  };

  const stateDisplay = getStateDisplay();

  const copyInfoHash = async () => {
    try {
      await navigator.clipboard.writeText(infoHash);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) {
      console.error("Failed to copy info hash", e);
    }
  };

  return (
    <div className="p-3">
      {/* Name */}
      <div className="truncate text-text mb-2" title={name}>
        {name}
      </div>

      {/* Pieces canvas */}
      {(detailsResponse.total_pieces ?? 0) > 0 && (
        <div className="mb-2">
          <PiecesCanvas
            torrentId={torrentId}
            totalPieces={detailsResponse.total_pieces ?? 0}
            stats={statsResponse}
          />
        </div>
      )}

      {/* Stats row */}
      <div className="flex flex-wrap gap-x-4 gap-y-1">
        <span>
          <span className="text-text-tertiary">Status </span>
          <span className={stateDisplay.color}>{stateDisplay.text}</span>
        </span>
        <span>
          <span className="text-text-tertiary">Progress </span>
          <span className="text-text">{progressPercentage.toFixed(1)}%</span>
          <span className="text-text-tertiary">
            {" "}
            ({formatBytes(progressBytes)} / {formatBytes(totalBytes)})
          </span>
        </span>
        <span>
          <span className="text-text-tertiary">Down </span>
          <span className="text-text">{downloadSpeed}</span>
        </span>
        <span>
          <span className="text-text-tertiary">Up </span>
          <span className="text-text">
            {uploadSpeed} (total {formatBytes(totalUploadedBytes)})
          </span>
        </span>
        <span>
          <span className="text-text-tertiary">ETA </span>
          <span className="text-text">{finished ? "Complete" : eta}</span>
        </span>
        <span>
          <span className="text-text-tertiary">Peers </span>
          <span className="text-text">
            {peersConnected}/{peersSeen}
          </span>
        </span>
      </div>

      {/* Info hash */}
      <div className="mt-2 flex items-center gap-1">
        <span className="text-text-tertiary">Hash</span>
        <code className="font-mono text-text-tertiary truncate flex-1">
          {infoHash}
        </code>
        <button
          onClick={copyInfoHash}
          className="p-0.5 text-text-tertiary hover:text-text transition-colors"
          title="Copy info hash"
        >
          {copied ? (
            <FaCheck className="w-3 h-3 text-success" />
          ) : (
            <FaCopy className="w-3 h-3" />
          )}
        </button>
      </div>

      {/* Error */}
      {error && <div className="mt-2 text-sm text-error">{error}</div>}
    </div>
  );
};
