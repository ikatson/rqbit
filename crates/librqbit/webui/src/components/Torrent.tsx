import { useContext, useEffect, useState } from "react";
import {
  TorrentDetails,
  TorrentId,
  TorrentStats,
  STATE_INITIALIZING,
  STATE_LIVE,
} from "../api-types";
import { APIContext, RefreshTorrentStatsContext } from "../context";
import { customSetInterval } from "../helper/customSetInterval";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { TorrentRow } from "./TorrentRow";

export const Torrent: React.FC<{
  id: number;
  torrent: TorrentId;
}> = ({ id, torrent }) => {
  const [detailsResponse, updateDetailsResponse] =
    useState<TorrentDetails | null>(null);
  const [statsResponse, updateStatsResponse] = useState<TorrentStats | null>(
    null
  );
  const [forceStatsRefresh, setForceStatsRefresh] = useState(0);
  const API = useContext(APIContext);

  const forceStatsRefreshCallback = () => {
    setForceStatsRefresh(forceStatsRefresh + 1);
  };

  // Update details once.
  useEffect(() => {
    if (detailsResponse === null) {
      return loopUntilSuccess(async () => {
        await API.getTorrentDetails(torrent.id).then(updateDetailsResponse);
      }, 1000);
    }
  }, [detailsResponse]);

  // Update stats once then forever.
  useEffect(
    () =>
      customSetInterval(async () => {
        const errorInterval = 10000;
        const liveInterval = 1000;
        const nonLiveInterval = 10000;

        return API.getTorrentStats(torrent.id)
          .then((stats) => {
            updateStatsResponse(stats);
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
            }
          );
      }, 0),
    [forceStatsRefresh]
  );

  return (
    <RefreshTorrentStatsContext.Provider
      value={{ refresh: forceStatsRefreshCallback }}
    >
      <TorrentRow
        id={id}
        detailsResponse={detailsResponse}
        statsResponse={statsResponse}
      />
    </RefreshTorrentStatsContext.Provider>
  );
};
