import {
  TorrentStats,
  STATE_INITIALIZING,
  STATE_PAUSED,
  STATE_ERROR,
} from "../api-types";
import { formatBytes } from "../helper/formatBytes";

export const Speed: React.FC<{ statsResponse: TorrentStats }> = ({
  statsResponse,
}) => {
  switch (statsResponse.state) {
    case STATE_PAUSED:
      return <span className="text-secondary">Paused</span>;
    case STATE_INITIALIZING:
      return <span className="text-warning">Checking files</span>;
    case STATE_ERROR:
      return <span className="text-error">Error</span>;
  }
  // Unknown state
  if (statsResponse.state != "live" || statsResponse.live === null) {
    return <span className="text-secondary">{statsResponse.state}</span>;
  }

  return (
    <>
      {!statsResponse.finished && (
        <span className="text-success">
          ↓ {statsResponse.live.download_speed?.human_readable}
        </span>
      )}
      <span className="text-primary">
        ↑ {statsResponse.live.upload_speed?.human_readable}
        {statsResponse.live.snapshot.uploaded_bytes > 0 && (
          <span className="text-secondary">
            ({formatBytes(statsResponse.live.snapshot.uploaded_bytes)})
          </span>
        )}
      </span>
    </>
  );
};
