import {
  TorrentListItem,
  STATE_INITIALIZING,
  STATE_LIVE,
  STATE_PAUSED,
} from "../../api-types";
import { formatBytes } from "../../helper/formatBytes";
import { getCompletionETA } from "../../helper/getCompletionETA";
import { PiecesCanvas } from "./PiecesCanvas";

interface OverviewTabProps {
  torrent: TorrentListItem | null;
}

// Labeled value - the building block for all stats
const LV: React.FC<{
  label: string;
  value: React.ReactNode;
  mono?: boolean;
}> = ({ label, value, mono }) => (
  <>
    <span className="text-tertiary">{label} </span>
    <span className={mono ? "font-mono" : ""}>{value}</span>
  </>
);

export const OverviewTab: React.FC<OverviewTabProps> = ({ torrent }) => {
  const statsResponse = torrent?.stats ?? null;

  if (!torrent || !statsResponse) {
    return <div className="p-3 text-tertiary">Loading...</div>;
  }

  const name = torrent.name ?? "";
  const infoHash = torrent.info_hash;
  const state = statsResponse.state;
  const error = statsResponse.error;
  const totalBytes = statsResponse.total_bytes ?? 1;
  const progressBytes = statsResponse.progress_bytes ?? 0;
  const finished = statsResponse.finished || false;

  const totalPieces = torrent.total_pieces ?? 0;
  const downloadedPieces =
    statsResponse.live?.snapshot.downloaded_and_checked_pieces ?? 0;
  const pieceSize = totalPieces > 0 ? totalBytes / totalPieces : 0;

  const totalUploadedBytes = statsResponse.live?.snapshot.uploaded_bytes ?? 0;

  const progressPct = error
    ? 100
    : totalBytes === 0
      ? 100
      : (progressBytes / totalBytes) * 100;

  const downSpeed = statsResponse.live?.download_speed?.human_readable ?? "-";
  const upSpeed = statsResponse.live?.upload_speed?.human_readable ?? "-";
  const eta = getCompletionETA(statsResponse);

  const peers = statsResponse.live?.snapshot.peer_stats;

  const stateDisplay = (() => {
    if (error) return { text: "Error", color: "text-error" };
    if (state === STATE_INITIALIZING)
      return { text: "Initializing", color: "text-warning" };
    if (state === STATE_PAUSED)
      return { text: "Paused", color: "text-secondary" };
    if (state === STATE_LIVE && finished)
      return { text: "Seeding", color: "text-success" };
    if (state === STATE_LIVE)
      return { text: "Downloading", color: "text-primary" };
    return { text: state, color: "text-secondary" };
  })();

  return (
    <div className="p-3 flex flex-col gap-3 text-sm">
      {/* Header: Name + Status */}
      <div className="flex items-center gap-3">
        <span className="truncate font-medium flex-1" title={name}>
          {name}
        </span>
        <span className={`shrink-0 ${stateDisplay.color}`}>
          {stateDisplay.text}
        </span>
      </div>

      {/* Pieces visualization */}
      {totalPieces > 0 && (
        <div>
          <PiecesCanvas
            torrentId={torrent.id}
            totalPieces={totalPieces}
            stats={statsResponse}
          />
        </div>
      )}

      {/* Main stats line */}
      <div className="flex flex-wrap gap-x-4 gap-y-1">
        <span>
          <LV label="Progress" value={`${progressPct.toFixed(1)}%`} />
          <span className="text-tertiary">
            {" "}
            ({formatBytes(progressBytes)}/{formatBytes(totalBytes)})
          </span>
        </span>
        <span>
          <LV label="Down" value={downSpeed} />
        </span>
        <span>
          <LV label="Up" value={upSpeed} />
          <span className="text-tertiary">
            {" "}
            ({formatBytes(totalUploadedBytes)} total)
          </span>
        </span>
        <span>
          <LV label="ETA" value={finished ? "Complete" : eta} />
        </span>
      </div>

      {/* Pieces + Peers line */}
      <div className="flex flex-wrap gap-x-4 gap-y-1">
        {totalPieces > 0 && (
          <span>
            <LV
              label="Pieces"
              value={`${downloadedPieces.toLocaleString()}/${totalPieces.toLocaleString()}`}
            />
            {pieceSize > 0 && (
              <span className="text-tertiary">
                {" "}
                ({formatBytes(pieceSize)} each)
              </span>
            )}
          </span>
        )}
        {peers && (
          <span>
            <LV label="Peers" value="" />
            <span className="text-success">{peers.live}</span>
            <span className="text-tertiary"> live, </span>
            <span className="text-primary">{peers.connecting}</span>
            <span className="text-tertiary"> connecting, </span>
            <span className="text-warning">{peers.queued}</span>
            <span className="text-tertiary"> queued, </span>
            <span>{peers.seen}</span>
            <span className="text-tertiary"> seen, </span>
            <span className="text-error">{peers.dead}</span>
            <span className="text-tertiary"> dead</span>
          </span>
        )}
      </div>

      {/* Metadata */}
      <div className="flex flex-col gap-1">
        <div className="truncate">
          <LV label="Hash" value={infoHash} mono />
        </div>
        <div className="truncate">
          <LV label="Output" value={torrent.output_folder} mono />
        </div>
      </div>

      {/* Error */}
      {error && <div className="text-error">{error}</div>}
    </div>
  );
};
