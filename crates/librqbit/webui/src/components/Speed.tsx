import {
  TorrentStats,
  STATE_INITIALIZING,
  STATE_PAUSED,
  STATE_ERROR,
} from "../api-types";
import { formatBytes } from "../helper/formatBytes";

export const Speed: React.FC<{ stats: TorrentStats }> = ({ stats }) => {
  switch (stats.state) {
    case STATE_PAUSED:
      return "Paused";
    case STATE_INITIALIZING:
      return "Checking files";
    case STATE_ERROR:
      return "Error";
  }
  // Unknown state
  if (stats.state != "live" || stats.live === null) {
    return stats.state;
  }

  return (
    <>
      {!stats.finished && (
        <div className="download-speed">
          ↓ {stats.live.download_speed?.human_readable}
        </div>
      )}
      <div className="upload-speed">
        ↑ {stats.live.upload_speed?.human_readable}
        {stats.live.snapshot.uploaded_bytes > 0 && (
          <span>({formatBytes(stats.live.snapshot.uploaded_bytes)})</span>
        )}
      </div>
    </>
  );
};
