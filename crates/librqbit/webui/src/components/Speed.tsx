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
      return "Paused";
    case STATE_INITIALIZING:
      return "Checking files";
    case STATE_ERROR:
      return "Error";
  }
  // Unknown state
  if (statsResponse.state != "live" || statsResponse.live === null) {
    return statsResponse.state;
  }

  return (
    <>
      {!statsResponse.finished && (
        <div className="download-speed">
          ↓ {statsResponse.live.download_speed?.human_readable}
        </div>
      )}
      <div className="upload-speed">
        ↑ {statsResponse.live.upload_speed?.human_readable}
        {statsResponse.live.snapshot.uploaded_bytes > 0 && (
          <span>
            ({formatBytes(statsResponse.live.snapshot.uploaded_bytes)})
          </span>
        )}
      </div>
    </>
  );
};
