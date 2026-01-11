import { useState } from "react";
import { TorrentDetails, TorrentStats, STATE_INITIALIZING, STATE_LIVE, STATE_PAUSED, STATE_ERROR } from "../../api-types";
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
    return (
      <div className="p-4 text-gray-400 dark:text-slate-500">
        Loading...
      </div>
    );
  }

  const name = torrentDisplayName(detailsResponse);
  const infoHash = detailsResponse.info_hash;
  const state = statsResponse.state;
  const error = statsResponse.error;
  const totalBytes = statsResponse.total_bytes ?? 1;
  const progressBytes = statsResponse.progress_bytes ?? 0;
  const finished = statsResponse.finished || false;

  const progressPercentage = error
    ? 100
    : totalBytes === 0
      ? 100
      : (progressBytes / totalBytes) * 100;

  const downloadSpeed = statsResponse.live?.download_speed?.human_readable ?? "-";
  const uploadSpeed = statsResponse.live?.upload_speed?.human_readable ?? "-";
  const eta = getCompletionETA(statsResponse);

  const peerStats = statsResponse.live?.snapshot.peer_stats;
  const peersConnected = peerStats?.live ?? 0;
  const peersSeen = peerStats?.seen ?? 0;

  const getStateDisplay = () => {
    if (error) return { text: "Error", color: "text-red-500" };
    if (state === STATE_INITIALIZING) return { text: "Initializing", color: "text-yellow-500" };
    if (state === STATE_PAUSED) return { text: "Paused", color: "text-gray-500" };
    if (state === STATE_LIVE && finished) return { text: "Seeding", color: "text-green-500" };
    if (state === STATE_LIVE) return { text: "Downloading", color: "text-blue-500" };
    return { text: state, color: "text-gray-500" };
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
    <div className="p-4 grid grid-cols-2 lg:grid-cols-4 gap-4 text-sm">
      <div className="col-span-2 lg:col-span-4">
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">Name</label>
        <div className="font-medium text-gray-900 dark:text-slate-100 truncate" title={name}>
          {name}
        </div>
      </div>

      <div>
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">Status</label>
        <div className={`font-medium ${stateDisplay.color}`}>
          {stateDisplay.text}
        </div>
      </div>

      <div>
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">Progress</label>
        <div className="font-medium text-gray-900 dark:text-slate-100">
          {progressPercentage.toFixed(1)}% ({formatBytes(progressBytes)} / {formatBytes(totalBytes)})
        </div>
      </div>

      {(detailsResponse.total_pieces ?? 0) > 0 && (
        <div className="col-span-2 lg:col-span-4">
          <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">
            Pieces ({detailsResponse.total_pieces})
          </label>
          <div className="mt-1">
            <PiecesCanvas
              torrentId={torrentId}
              totalPieces={detailsResponse.total_pieces ?? 0}
              stats={statsResponse}
            />
          </div>
        </div>
      )}

      <div>
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">Download</label>
        <div className="font-medium text-gray-900 dark:text-slate-100">{downloadSpeed}</div>
      </div>

      <div>
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">Upload</label>
        <div className="font-medium text-gray-900 dark:text-slate-100">{uploadSpeed}</div>
      </div>

      <div>
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">ETA</label>
        <div className="font-medium text-gray-900 dark:text-slate-100">
          {finished ? "Complete" : eta}
        </div>
      </div>

      <div>
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">Peers</label>
        <div className="font-medium text-gray-900 dark:text-slate-100">
          {peersConnected} connected / {peersSeen} seen
        </div>
      </div>

      <div className="col-span-2">
        <label className="text-xs text-gray-500 dark:text-slate-400 uppercase tracking-wide">Info Hash</label>
        <div className="flex items-center gap-2">
          <code className="font-mono text-xs text-gray-700 dark:text-slate-300 truncate flex-1">
            {infoHash}
          </code>
          <button
            onClick={copyInfoHash}
            className="p-1 text-gray-400 hover:text-gray-600 dark:hover:text-slate-300 transition-colors"
            title="Copy info hash"
          >
            {copied ? <FaCheck className="w-3 h-3 text-green-500" /> : <FaCopy className="w-3 h-3" />}
          </button>
        </div>
      </div>

      {error && (
        <div className="col-span-2 lg:col-span-4">
          <label className="text-xs text-red-500 uppercase tracking-wide">Error</label>
          <div className="text-red-500">{error}</div>
        </div>
      )}
    </div>
  );
};
