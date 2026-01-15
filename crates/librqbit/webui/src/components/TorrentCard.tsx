import { useContext, useEffect, useState } from "react";
import { TorrentListItem } from "../api-types";
import { APIContext } from "../context";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { TorrentCardContent } from "./TorrentCardContent";
import { useTorrentStore } from "../stores/torrentStore";

export const TorrentCard: React.FC<{
  torrent: TorrentListItem;
  hidden?: boolean;
}> = ({ torrent, hidden }) => {
  const API = useContext(APIContext);
  const [fetchDetails, setFetchDetails] = useState(false);

  const cachedDetails = useTorrentStore((state) => state.getDetails(torrent.id));
  const setDetails = useTorrentStore((state) => state.setDetails);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  // Fetch details when requested
  useEffect(() => {
    if (!fetchDetails) return;
    return loopUntilSuccess(async () => {
      const details = await API.getTorrentDetails(torrent.id);
      setDetails(torrent.id, details);
      setFetchDetails(false);
    }, 1000);
  }, [fetchDetails, torrent.id]);

  const onExtendedViewOpen = () => {
    if (!cachedDetails) {
      setFetchDetails(true);
    }
  };

  const forceRefreshCallback = () => {
    refreshTorrents();
    // Re-fetch details if we had them
    if (cachedDetails) {
      setFetchDetails(true);
    }
  };

  return (
    <div className={hidden ? "hidden" : ""}>
      <TorrentCardContent
        torrent={torrent}
        detailsResponse={cachedDetails}
        onExtendedViewOpen={onExtendedViewOpen}
        onRefresh={forceRefreshCallback}
      />
    </div>
  );
};
