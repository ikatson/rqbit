import {
  MdDownload,
  MdOutlineMotionPhotosPaused,
  MdCheck,
} from "react-icons/md";

import { GoClock, GoFile, GoPeople } from "react-icons/go";
import {
  TorrentDetails,
  TorrentStats,
  STATE_INITIALIZING,
  STATE_LIVE,
} from "../api-types";
import { TorrentActions } from "./buttons/TorrentActions";
import { ProgressBar } from "./ProgressBar";
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

  const isDownloading = !!statsResponse?.live;

  const formatPeersString = () => {
    let peer_stats = statsResponse?.live?.snapshot.peer_stats;
    if (!peer_stats) {
      return "";
    }
    return `${peer_stats.live} / ${peer_stats.seen}`;
  };

  return (
    <section className="flex flex-col md:flex-row items-center gap-2 border p-2 border-gray-200 rounded-xl shadow-xs hover:drop-shadow-sm">
      {/* Icon */}
      <div className="p-1">
        {finished ? (
          <MdCheck className="w-10 h-10" color="green" />
        ) : isDownloading ? (
          <MdDownload className="w-10 h-10" color="green" />
        ) : (
          <MdOutlineMotionPhotosPaused className="w-10 h-10" />
        )}
      </div>
      {/* Name, progress, stats */}
      <div className="w-full flex flex-col gap-2">
        {detailsResponse && (
          <div>
            <p className="text-left text-lg text-gray-900 text-ellipsis break-all">
              {getLargestFileName(detailsResponse)}
            </p>
            <div
              id="tooltip"
              className="hidden bg-gray-800 text-white p-2 rounded absolute group-hover:block"
            >
              {getLargestFileName(detailsResponse)}
            </div>
          </div>
        )}
        {error ? (
          <p className="text-danger">
            <strong>Error:</strong> {error}
          </p>
        ) : (
          <>
            <div className="mt-1">
              <ProgressBar
                error={error}
                now={progressPercentage}
                finished={finished}
                initializaion={state == STATE_INITIALIZING}
                live={state === STATE_LIVE}
              />
            </div>
            <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-2 sm:flex-wrap items-center text-sm text-nowrap font-medium text-gray-500">
              <div className="flex gap-2 items-center">
                <GoPeople /> {formatPeersString().toString()}
              </div>
              <div className="flex gap-2 items-center">
                <GoFile />
                <div>
                  {formatBytes(progressBytes)}/{formatBytes(totalBytes)}
                </div>
              </div>
              {statsResponse && (
                <>
                  <div className="flex gap-2 items-center">
                    <GoClock />
                    {getCompletionETA(statsResponse)}
                  </div>
                  <div className="flex gap-2 items-center">
                    <Speed statsResponse={statsResponse} />
                  </div>
                </>
              )}
            </div>
          </>
        )}
      </div>
      {/* Actions */}
      {statsResponse && (
        <div className="">
          <TorrentActions id={id} statsResponse={statsResponse} />
        </div>
      )}
    </section>
  );
};
