import { useContext, useEffect, useState } from "react";
import {
  TorrentDetails,
  STATE_INITIALIZING,
  STATE_LIVE,
  TorrentIdWithStats,
} from "../api-types";
import { APIContext } from "../context";
import { customSetInterval } from "../helper/customSetInterval";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { TorrentRow, CompactTorrentRow } from "./TorrentRow";
import { useTorrentStore } from "../stores/torrentStore";
import { ViewModeContext } from "../stores/viewMode";

export const Torrent: React.FC<{
  torrent: TorrentIdWithStats;
  onClick?: () => void;
}> = ({ torrent, onClick }) => {
  const selectedTorrent = useTorrentStore((state) => state.selectedTorrent);
  const { compact } = useContext(ViewModeContext);
  const [currentDetailsResponse, updateDetailsResponse] =
    useState<TorrentDetails | null>(null);
  const API = useContext(APIContext);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  // Update details once then when asked for.
  useEffect(() => {
    if (compact) return;
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrent.id).then(updateDetailsResponse);
    }, 1000);
  }, [compact, torrent.id]);

  return (
    <>
      {compact ? (
        <CompactTorrentRow
          torrent={torrent}
          onClick={onClick!}
          selected={selectedTorrent?.id === torrent.id}
        />
      ) : (
        <TorrentRow
          torrent={torrent}
          detailsResponse={currentDetailsResponse}
        />
      )}
    </>
  );
};
