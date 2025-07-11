import { useState } from "react";
import { TorrentDetails, TorrentIdWithStats } from "../api-types";
import { FileListInput } from "./FileListInput";
import { formatBytes } from "../helper/formatBytes";
import { TorrentActions } from "./buttons/TorrentActions";
import { PeerTable } from "./PeerTable";
import { Tab, Tabs } from "./tabs/Tabs";

const TABS = ["Files", "Peers"];

export const TorrentDetailsPane: React.FC<{
  torrent: TorrentIdWithStats;
  details: TorrentDetails | null;
}> = ({ details, torrent }) => {
  const [extendedView, setExtendedView] = useState(false);

  return (
    <div>
      <div className="mb-2 p-2 bg-gray-100 dark:bg-gray-800 rounded-md text-xs">
        <p className="font-bold text-sm">{torrent.name}</p>
        <p>ID: {torrent.id}</p>
        <p>Size: {formatBytes(torrent.stats.total_bytes)}</p>
        <p>Info Hash: {torrent.info_hash}</p>
        <p>Output folder: {torrent.output_folder}</p>
      </div>
      <div className="mt-2">
        <TorrentActions
          id={torrent.id}
          statsResponse={torrent.stats}
          detailsResponse={details}
          extendedView={extendedView}
          setExtendedView={setExtendedView}
        />
      </div>
      <Tabs tabs={TABS}>
        <Tab>
          {details && (
            <FileListInput
              torrentId={torrent.id}
              torrentDetails={details}
              torrentStats={torrent.stats}
              selectedFiles={new Set(details.files.map((_, i) => i))}
              setSelectedFiles={() => {}}
              disabled={false}
              allowStream={true}
              showProgressBar={true}
            />
          )}
        </Tab>
        <Tab>
          <PeerTable torrent={torrent} />
        </Tab>
      </Tabs>
    </div>
  );
};
