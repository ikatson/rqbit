import { GoClock, GoFile, GoPeople } from "react-icons/go";
import {
  TorrentDetails,
  TorrentListItem,
  STATE_INITIALIZING,
} from "../api-types";
import { TorrentActions } from "./buttons/TorrentActions";
import { ProgressBar } from "./ProgressBar";
import { Speed } from "./Speed";
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { StatusIcon } from "./StatusIcon";

export const TorrentCardContent: React.FC<{
  torrent: TorrentListItem;
  detailsResponse: TorrentDetails | null;
}> = ({ torrent, detailsResponse }) => {
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
    <div className="flex flex-col border p-2 border-divider rounded-xl shadow-xs hover:drop-shadow-sm bg-surface-raised">
      <section className="flex flex-col lg:flex-row items-center gap-2">
        {/* Icon */}
        <div className="hidden md:block">{statusIcon("w-10 h-10")}</div>
        {/* Name, progress, stats */}
        <div className="w-full flex flex-col gap-2">
          <div className="flex items-center gap-2">
            <div className="md:hidden">{statusIcon("w-5 h-5")}</div>
            <div className="text-left lg:text-lg text-ellipsis break-all">
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
              <div className="grid grid-cols-2 lg:grid-cols-4 gap-2 sm:flex-wrap items-center text-nowrap font-medium text-secondary">
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
            <TorrentActions
              id={id}
              statsResponse={statsResponse}
              detailsResponse={detailsResponse}
            />
          </div>
        )}
      </section>
    </div>
  );
};
