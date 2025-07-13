import { useContext, useState } from "react";
import { TorrentDetails, TorrentIdWithStats } from "../api-types";
import { FileListInput } from "./FileListInput";
import { TorrentActions } from "./buttons/TorrentActions";
import { PeerTable } from "./PeerTable";
import { Tab, Tabs } from "./tabs/Tabs";
import { TorrentDetailsOverviewTab } from "./TorrentDetailsOverviewTab";
import { ViewModeContext } from "../stores/viewMode";
import { ManagedTorrentFileListInput } from "./ManagedTorrentFileListInput";

export const TorrentDetailsPane: React.FC<{
  torrent: TorrentIdWithStats;
}> = ({ torrent }) => {
  const [extendedView, setExtendedView] = useState(false);
  const { compact } = useContext(ViewModeContext);

  return (
    <div>
      <div className="p-2 bg-gray-100 dark:bg-gray-800">
        <TorrentActions
          torrent={torrent}
          extendedView={extendedView}
          setExtendedView={setExtendedView}
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
