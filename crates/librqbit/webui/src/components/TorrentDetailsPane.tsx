import { useContext, useMemo, useState } from "react";
import { TorrentDetails, TorrentIdWithStats } from "../api-types";
import { FileListInput } from "./FileListInput";
import { TorrentActions } from "./TorrentActions";
import { PeerTable } from "./PeerTable";
import { Tab, Tabs } from "./tabs/Tabs";
import { TorrentDetailsOverviewTab } from "./TorrentDetailsOverviewTab";
import { ViewModeContext } from "../stores/viewMode";
import { ManagedTorrentFileListInput } from "./ManagedTorrentFileListInput";
import { useTorrentStore } from "../stores/torrentStore";
import { TorrentActionsMulti } from "./TorrentActionsMulti";

const noop = () => {};

export const TorrentDetailsPaneSingleSelected: React.FC<{ id: number }> = ({
  id,
}) => {
  const torrent = useTorrentStore((state) =>
    state.torrents?.find((t) => t.id === id),
  );
  if (!torrent) return null;
  return (
    <div>
      <div className="p-2 bg-gray-100 dark:bg-gray-800">
        <TorrentActions
          torrent={torrent}
          extendedView={false}
          setExtendedView={noop}
        />
      </div>
      <Tabs tabs={["Overview", "Files", "Peers"]}>
        <Tab>
          <TorrentDetailsOverviewTab torrent={torrent} />
        </Tab>
        <Tab>
          <ManagedTorrentFileListInput torrent={torrent} />
        </Tab>
        <Tab>
          <PeerTable torrent={torrent} />
        </Tab>
      </Tabs>
    </div>
  );
};

export const TorrentDetailsPane: React.FC<{}> = () => {
  const selectedTorrentIds = useTorrentStore(
    (state) => state.selectedTorrentIds,
  );
  if (selectedTorrentIds.length === 0) {
    return null;
  } else if (selectedTorrentIds.length === 1) {
    return <TorrentDetailsPaneSingleSelected id={selectedTorrentIds[0]} />;
  } else {
    return (
      <div className="p-2 bg-gray-100 dark:bg-gray-800">
        <TorrentActionsMulti torrentIds={selectedTorrentIds} />
      </div>
    );
  }
};
