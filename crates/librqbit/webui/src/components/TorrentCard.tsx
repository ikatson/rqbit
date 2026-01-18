import { TorrentListItem } from "../api-types";
import { TorrentCardContent } from "./TorrentCardContent";

export const TorrentCard: React.FC<{
  torrent: TorrentListItem;
  hidden?: boolean;
}> = ({ torrent, hidden }) => {
  return (
    <div className={hidden ? "hidden" : ""}>
      <TorrentCardContent torrent={torrent} />
    </div>
  );
};
