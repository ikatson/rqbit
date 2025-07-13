import { GoClock, GoFile, GoPeople } from "react-icons/go";
import { STATE_INITIALIZING, TorrentIdWithStats } from "../api-types";
import { TorrentActions } from "./buttons/TorrentActions";
import { ProgressBar } from "./ProgressBar";
import { Speed } from "./Speed";
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { StatusIcon } from "./StatusIcon";
import { useContext, useState } from "react";
import { APIContext } from "../context";
import { useTorrentStore } from "../stores/torrentStore";
import { ManagedTorrentFileListInput } from "./ManagedTorrentFileListInput";

export const TorrentRow: React.FC<{
  torrent: TorrentIdWithStats;
}> = ({ torrent }) => {
  const stats = torrent.stats;
  const state = stats.state ?? "";
  const error = stats.error ?? null;
  const totalBytes = stats.total_bytes ?? 1;
  const progressBytes = stats.progress_bytes ?? 0;
  const finished = stats.finished || false;
  const progressPercentage = error
    ? 100
    : totalBytes == 0
      ? 100
      : (progressBytes / totalBytes) * 100;
  const refresh = useTorrentStore((state) => state.refreshTorrents);

  const formatPeersString = () => {
    let peer_stats = stats?.live?.snapshot.peer_stats;
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
        live={!!stats?.live}
        finished={finished}
      />
    );
  };

  const API = useContext(APIContext);
  const [extendedView, setExtendedView] = useState(false);

  return (
    <div className="flex flex-col border p-2 border-gray-200 rounded-xl shadow-xs hover:drop-shadow-sm dark:bg-slate-800 dark:border-slate-900">
      <section className="flex flex-col lg:flex-row items-center gap-2">
        {/* Icon */}
        <div className="hidden md:block">{statusIcon("w-10 h-10")}</div>
        {/* Name, progress, stats */}
        <div className="w-full flex flex-col gap-2">
          <div className="flex items-center gap-2">
            <div className="md:hidden">{statusIcon("w-5 h-5")}</div>
            <div className="text-left text-sm lg:text-lg text-gray-900 text-ellipsis break-all dark:text-slate-200">
              {torrent.name}
            </div>
          </div>
          {error ? (
            <p className="text-red-500 text-sm">
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
              <div className="grid grid-cols-2 lg:grid-cols-4 gap-2 sm:flex-wrap items-center text-sm text-nowrap font-medium text-gray-500">
                <div className="flex gap-2 items-center">
                  <GoPeople /> {formatPeersString()}
                </div>
                <div className="flex gap-2 items-center">
                  <GoFile />
                  <div>
                    {formatBytes(progressBytes)}/{formatBytes(totalBytes)}
                  </div>
                </div>
                <div className="flex gap-2 items-center">
                  <GoClock />
                  {getCompletionETA(torrent.stats)}
                </div>
                <div className="flex gap-2 items-center">
                  <Speed stats={torrent.stats} />
                </div>
              </div>
            </>
          )}
        </div>
        {/* Actions */}
        <TorrentActions
          torrent={torrent}
          extendedView={extendedView}
          setExtendedView={setExtendedView}
        />
      </section>

      {extendedView && <ManagedTorrentFileListInput torrent={torrent} />}
    </div>
  );
};
