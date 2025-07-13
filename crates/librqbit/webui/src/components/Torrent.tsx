import { useContext } from "react";
import { TorrentIdWithStats } from "../api-types";
import { TorrentRow } from "./TorrentRow";
import { CompactTorrentRow } from "./CompactTorrentRow";
import { ViewModeContext } from "../stores/viewMode";

export const Torrent: React.FC<{
  torrent: TorrentIdWithStats;
}> = ({ torrent }) => {
  const { compact } = useContext(ViewModeContext);

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
