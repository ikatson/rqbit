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
}> = ({ torrent }) => {
  const { compact } = useContext(ViewModeContext);
  const [currentDetailsResponse, updateDetailsResponse] =
    useState<TorrentDetails | null>(null);
  const API = useContext(APIContext);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  return (
    <>
      {compact ? (
        <CompactTorrentRow torrent={torrent} />
      ) : (
        <TorrentRow torrent={torrent} />
      )}
    </>
  );
};
