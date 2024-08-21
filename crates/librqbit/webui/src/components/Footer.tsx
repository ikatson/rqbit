import { formatBytes } from "../helper/formatBytes";
import { formatSecondsToTime } from "../helper/formatSecondsToTime";
import { useStatsStore } from "../stores/statsStore";

const FooterPiece: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  return <div className="p-1">{children}</div>;
};

export const Footer: React.FC<{}> = () => {
  let stats = useStatsStore((stats) => stats.stats);
  return (
    <div className="sticky bottom-0 bg-white/10 dark:text-gray-200 backdrop-blur text-nowrap text-xs font-medium text-gray-500 flex gap-x-1 lg:gap-x-5 justify-evenly flex-wrap">
      <FooterPiece>
        ↓ {stats.download_speed.human_readable} (
        {formatBytes(stats.fetched_bytes)})
      </FooterPiece>
      <FooterPiece>
        ↑ {stats.upload_speed.human_readable} (
        {formatBytes(stats.uploaded_bytes)})
      </FooterPiece>
      <FooterPiece>up {formatSecondsToTime(stats.uptime_seconds)}</FooterPiece>
    </div>
  );
};
