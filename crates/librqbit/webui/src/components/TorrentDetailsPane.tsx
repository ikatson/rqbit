import { useContext, useState } from "react";
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

export const TorrentDetailsPane: React.FC<{}> = () => {
  const selectedTorrents = useTorrentStore((state) =>
    state.selectedTorrentIds
      .map((id) => (state.torrents || []).find((t) => t.id === id))
      .filter((t) => t !== undefined),
  );
  if (selectedTorrents.length === 0) {
    return null;
  } else if (selectedTorrents.length === 1) {
    const torrent = selectedTorrents[0];
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
  } else {
    return (
      <div className="p-2 bg-gray-100 dark:bg-gray-800">
        <TorrentActionsMulti torrents={selectedTorrents} />
      </div>
    );
  }
};
