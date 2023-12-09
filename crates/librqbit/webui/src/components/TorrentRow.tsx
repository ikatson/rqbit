import { ProgressBar } from "react-bootstrap";
import {
  MdDownload,
  MdOutlineMotionPhotosPaused,
  MdThumbUp,
} from "react-icons/md";

import { GoClock, GoFile, GoPeople } from "react-icons/go";
import {
  TorrentDetails,
  TorrentStats,
  STATE_INITIALIZING,
  STATE_LIVE,
} from "../api-types";
import { TorrentActions } from "./TorrentActions";
import { Speed } from "./Speed";
import { formatBytes } from "../helper/formatBytes";
import { getLargestFileName } from "../helper/getLargestFileName";
import { getCompletionETA } from "../helper/getCompletionETA";

export const TorrentRow: React.FC<{
  id: number;
  detailsResponse: TorrentDetails | null;
  statsResponse: TorrentStats | null;
}> = ({ id, detailsResponse, statsResponse }) => {
  const state = statsResponse?.state ?? "";
  const error = statsResponse?.error;
  const totalBytes = statsResponse?.total_bytes ?? 1;
  const progressBytes = statsResponse?.progress_bytes ?? 0;
  const finished = statsResponse?.finished || false;
  const progressPercentage = error ? 100 : (progressBytes / totalBytes) * 100;
  const isAnimated =
    (state == STATE_INITIALIZING || state == STATE_LIVE) && !finished;
  const progressLabel = error ? "Error" : `${progressPercentage.toFixed(2)}%`;
  const progressBarVariant = error
    ? "danger"
    : finished
      ? "success"
      : state == STATE_INITIALIZING
        ? "warning"
        : "primary";
  const isDownloading = !!statsResponse?.live;

  const formatPeersString = () => {
    let peer_stats = statsResponse?.live?.snapshot.peer_stats;
    if (!peer_stats) {
      return "";
    }
    return `${peer_stats.live} / ${peer_stats.seen}`;
  };

  let classNames = [];

  if (error) {
    classNames.push("bg-warning");
  } else {
    if (id % 2 == 0) {
      classNames.push("bg-light");
    }
  }

  return (
    <section className="w-full grid bg-white border grid-cols-12 p-2 border-gray-200 rounded-xl shadow-xs hover:drop-shadow-sm items-center gap-2">
      <div className="col-span-1 p-2">
        {finished ? (
          <MdThumbUp className="w-12 h-full" color="green" />
        ) : isDownloading ? (
          <MdDownload className="w-12 h-full" color="green" />
        ) : (
          <MdOutlineMotionPhotosPaused className="w-12 h-full" />
        )}
      </div>
      <div className="text-left col-span-10">
        {detailsResponse && (
          <p className="text-left text-xl font-bold tracking-tight text-gray-900 truncate">
            {getLargestFileName(detailsResponse)}
          </p>
        )}
        {error ? (
          <p className="text-danger">
            <strong>Error:</strong> {error}
          </p>
        ) : (
          <>
            <div className="mt-3 mb-1">
              <ProgressBar
                now={progressPercentage}
                label={progressLabel}
                animated={isAnimated}
                variant={progressBarVariant}
              />
            </div>
            <div className="grid grid-flow-col gap-4 w-full justify-start  text-sm font-medium text-gray-500">
              <p className="col-span-2 flex items-center">
                <GoPeople /> {formatPeersString().toString()}
              </p>
              <p className="col-span-2 flex items-center">
                <GoFile />
                {formatBytes(progressBytes)}/{formatBytes(totalBytes)}
              </p>
              {statsResponse && (
                <>
                  <p className="col-span-2 flex items-center">
                    <GoClock />
                    {getCompletionETA(statsResponse)}
                  </p>
                  <Speed statsResponse={statsResponse} />
                </>
              )}
            </div>
          </>
        )}
      </div>
      {statsResponse && (
        <div className="col-span-1">
          <TorrentActions id={id} statsResponse={statsResponse} />
        </div>
      )}
    </section>
  );
};
