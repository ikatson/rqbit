import { GoClock, GoFile, GoPeople } from "react-icons/go";
import { TorrentListItem, STATE_INITIALIZING } from "../api-types";
import { TorrentActions } from "./buttons/TorrentActions";
import { ProgressBar } from "./ProgressBar";
import { Speed } from "./Speed";
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { StatusIcon } from "./StatusIcon";

export const TorrentCardContent: React.FC<{
  torrent: TorrentListItem;
}> = ({ torrent }) => {
  const id = torrent.id;
  const statsResponse = torrent.stats ?? null;
  const state = statsResponse?.state ?? "";
  const error = statsResponse?.error ?? null;
  const totalBytes = statsResponse?.total_bytes ?? 1;
  const progressBytes = statsResponse?.progress_bytes ?? 0;
  const finished = statsResponse?.finished || false;
  const progressPercentage = error
    ? 100
    : totalBytes == 0
      ? 100
      : (progressBytes / totalBytes) * 100;

  const formatPeersString = () => {
    let peer_stats = statsResponse?.live?.snapshot.peer_stats;
    if (!peer_stats) {
      return "";
    }
    return `${peer_stats.live} / ${peer_stats.seen}`;
  };

  const statusIcon = (className: string) => {
    return (
      <StatusIcon
        className={className}
        error={!!error}
        live={!!statsResponse?.live}
        finished={finished}
      />
    );
  };

  return (
    <div className="flex flex-col border p-1.5 sm:p-2 border-divider rounded-xl shadow-xs hover:drop-shadow-sm bg-surface-raised">
      <section className="flex flex-col lg:flex-row items-center gap-1.5 sm:gap-2">
        {/* Icon */}
        <div className="hidden md:block">{statusIcon("w-10 h-10")}</div>
        {/* Name, progress, stats */}
        <div className="w-full flex flex-col gap-1 sm:gap-2">
          <div className="flex items-center gap-1.5 sm:gap-2">
            <div className="md:hidden">
              {statusIcon("w-4 h-4 sm:w-5 sm:h-5")}
            </div>
            <div className="text-left text-sm sm:text-base lg:text-lg text-ellipsis break-all line-clamp-2 sm:line-clamp-none">
              {torrent.name}
            </div>
          </div>
          {error ? (
            <p className="text-error">
              <strong>Error:</strong> {error}
            </p>
          ) : (
            <>
              <div>
                <ProgressBar
                  now={progressPercentage}
                  label={error}
                  variant={
                    state == STATE_INITIALIZING
                      ? "warn"
                      : finished
                        ? "success"
                        : "info"
                  }
                />
              </div>
              <div className="flex flex-wrap gap-x-3 gap-y-0.5 sm:gap-x-4 sm:gap-y-1 items-center text-nowrap text-xs sm:text-sm font-medium text-secondary">
                <div className="flex gap-1 sm:gap-2 items-center">
                  <GoPeople className="w-3 h-3 sm:w-4 sm:h-4" />{" "}
                  {formatPeersString().toString()}
                </div>
                <div className="flex gap-1 sm:gap-2 items-center">
                  <GoFile className="w-3 h-3 sm:w-4 sm:h-4" />
                  <div>
                    {formatBytes(progressBytes)}/{formatBytes(totalBytes)}
                  </div>
                </div>
                {statsResponse && (
                  <>
                    <div className="flex gap-1 sm:gap-2 items-center">
                      <GoClock className="w-3 h-3 sm:w-4 sm:h-4" />
                      {getCompletionETA(statsResponse)}
                    </div>
                    <div className="flex gap-1 sm:gap-2 items-center">
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
            <TorrentActions torrent={{ ...torrent, stats: statsResponse }} />
          </div>
        )}
      </section>
    </div>
  );
};
