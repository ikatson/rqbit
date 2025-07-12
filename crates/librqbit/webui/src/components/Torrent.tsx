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
  selected?: boolean;
}> = ({ torrent, onClick, selected }) => {
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

  // Update stats once then forever.
  useEffect(() => {
    if (compact) return;
    return customSetInterval(async () => {
      const errorInterval = 10000;
      const liveInterval = 1000;
      const nonLiveInterval = 10000;

      return API.getTorrentStats(torrent.id)
        .then((stats) => {
          refreshTorrents();
          return stats;
        })
        .then(
          (stats) => {
            if (
              stats.state == STATE_INITIALIZING ||
              stats.state == STATE_LIVE
            ) {
              return liveInterval;
            }
            return nonLiveInterval;
          },
          () => {
            return errorInterval;
          },
        );
    }, 0);
  }, [compact, torrent.id, refreshTorrents]);

  return (
    <>
      {compact ? (
        <CompactTorrentRow
          torrent={torrent}
          onClick={onClick!}
          selected={selected!}
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
