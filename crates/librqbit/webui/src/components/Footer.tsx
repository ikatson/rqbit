import { formatBytes } from "../helper/formatBytes";
import { formatSecondsToTime } from "../helper/formatSecondsToTime";
import { useStatsStore } from "../stores/statsStore";

export const Footer: React.FC<{}> = () => {
  let stats = useStatsStore((stats) => stats.stats);
  return (
    <div className="sticky bottom-0 bg-white/10 dark:text-gray-200 backdrop-blur text-nowrap text-xs font-medium text-gray-500 flex p-2 gap-x-5 justify-evenly flex-wrap">
      <div>
        ↓ {stats.download_speed.human_readable} (
        {formatBytes(stats.fetched_bytes)})
      </div>
      <div>
        ↑ {stats.upload_speed.human_readable} (
        {formatBytes(stats.uploaded_bytes)})
      </div>
      <div>up {formatSecondsToTime(stats.uptime_seconds)}</div>
    </div>
  );
};
