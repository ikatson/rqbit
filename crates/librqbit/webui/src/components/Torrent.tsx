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
import { TorrentRow } from "./TorrentRow";

export const Torrent: React.FC<{
  torrent: TorrentIdWithStats;
}> = ({ torrent }) => {
  const [detailsResponse, updateDetailsResponse] =
    useState<TorrentDetails | null>(null);
  const API = useContext(APIContext);

  // Update details once then when asked for.
  useEffect(() => {
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrent.id).then(updateDetailsResponse);
    }, 1000);
  }, []);

  return <TorrentRow torrent={torrent} detailsResponse={detailsResponse} />;
};
