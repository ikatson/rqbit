import { useContext, useEffect, useState } from "react";
import {
  TorrentDetails,
  TorrentListItem,
} from "../api-types";
import { APIContext, RefreshTorrentStatsContext } from "../context";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { TorrentCardContent } from "./TorrentCardContent";
import { useTorrentStore } from "../stores/torrentStore";

export const TorrentCard: React.FC<{
  torrent: TorrentListItem;
}> = ({ torrent }) => {
  // Fetch full details (with files) only when needed for extended view
  const [detailsResponse, updateDetailsResponse] =
    useState<TorrentDetails | null>(null);
  const [fetchDetails, setFetchDetails] = useState(false);
  const API = useContext(APIContext);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  // Create a synthetic TorrentDetails from TorrentListItem for display
  // (without files - those are fetched separately when extended view is opened)
  const syntheticDetails: TorrentDetails = {
    name: torrent.name,
    info_hash: torrent.info_hash,
    files: detailsResponse?.files ?? [],
    total_pieces: torrent.total_pieces,
    output_folder: torrent.output_folder,
  };

  const forceRefreshCallback = () => {
    // Trigger a global refresh of all torrents
    refreshTorrents();
    // Also re-fetch details if we had them
    if (detailsResponse) {
      setFetchDetails(true);
    }
  };

  // Fetch details only when requested (for extended view with files)
  useEffect(() => {
    if (!fetchDetails) return;
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrent.id).then((details) => {
        updateDetailsResponse(details);
        setFetchDetails(false);
      });
    }, 1000);
  }, [fetchDetails, torrent.id]);

  const onExtendedViewOpen = () => {
    if (!detailsResponse) {
      setFetchDetails(true);
    }
  };

  return (
    <RefreshTorrentStatsContext.Provider
      value={{ refresh: forceRefreshCallback }}
    >
      <TorrentCardContent
        id={torrent.id}
        detailsResponse={syntheticDetails}
        statsResponse={torrent.stats ?? null}
        onExtendedViewOpen={onExtendedViewOpen}
      />
    </RefreshTorrentStatsContext.Provider>
  );
};
