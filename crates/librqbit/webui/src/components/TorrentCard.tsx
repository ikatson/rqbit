import { TorrentListItem } from "../api-types";
import { TorrentCardContent } from "./TorrentCardContent";
import { useTorrentStore } from "../stores/torrentStore";

export const TorrentCard: React.FC<{
  torrent: TorrentListItem;
  hidden?: boolean;
}> = ({ torrent, hidden }) => {
  const cachedDetails = useTorrentStore((state) =>
    state.getDetails(torrent.id),
  );

  return (
    <div className={hidden ? "hidden" : ""}>
      <TorrentCardContent torrent={torrent} detailsResponse={cachedDetails} />
    </div>
  );
};
