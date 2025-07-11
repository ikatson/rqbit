import { TorrentDetails, TorrentIdWithStats } from "../api-types";
import { formatBytes } from "../helper/formatBytes";

export const TorrentDetailsOverviewTab: React.FC<{
  torrent: TorrentIdWithStats;
  details: TorrentDetails | null;
}> = ({ details, torrent }) => {
  return (
    <div className="p-2 text-xs">
      <p>
        <b>Name:</b> {torrent.name}
      </p>
      <p>
        <b>ID:</b> {torrent.id}
      </p>
      <p>
        <b>Size:</b> {formatBytes(torrent.stats.total_bytes)}
      </p>
      <p>
        <b>Info Hash:</b> {torrent.info_hash}
      </p>
      <p>
        <b>Output folder:</b> {torrent.output_folder}
      </p>
    </div>
  );
};
