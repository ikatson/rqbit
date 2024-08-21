import { useStatsStore } from "../stores/statsStore";
import { Speed } from "./Speed";

export const Footer: React.FC<{}> = () => {
  let stats = useStatsStore((stats) => stats.stats);
  return (
    <div className="sticky bottom-0 bg-white/10 dark:text-gray-200 backdrop-blur text-nowrap text-xs font-medium text-gray-500 flex p-1 gap-x-3 justify-center">
      <div>↓ {stats.download_speed.human_readable}</div>
      <div>↑ {stats.upload_speed.human_readable}</div>
    </div>
  );
};
