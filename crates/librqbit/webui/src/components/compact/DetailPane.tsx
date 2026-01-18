import { useContext, useEffect, useState } from "react";
import { APIContext } from "../../context";
import { loopUntilSuccess } from "../../helper/loopUntilSuccess";
import { useUIStore } from "../../stores/uiStore";
import { useTorrentStore } from "../../stores/torrentStore";
import { OverviewTab } from "./OverviewTab";
import { FilesTab } from "./FilesTab";
import { PeersTab } from "./PeersTab";
import { TabButton, TabList } from "../Tabs";

type TabId = "overview" | "files" | "peers";

export const DetailPane: React.FC = () => {
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);
  const [activeTab, setActiveTab] = useState<TabId>("overview");

  const selectedArray = Array.from(selectedTorrentIds);
  const selectedCount = selectedArray.length;

  if (selectedCount === 0) {
    return (
      <div className="h-full border-t border-divider bg-surface-raised flex items-center justify-center">
        <p className="text-tertiary">Select a torrent to view details</p>
      </div>
    );
  }

  if (selectedCount > 1) {
    return (
      <div className="h-full border-t border-divider bg-surface-raised flex items-center justify-center">
        <p className="text-tertiary">{selectedCount} torrents selected</p>
      </div>
    );
  }

  const selectedId = selectedArray[0];

  return (
    <div className="h-full border-t border-divider flex flex-col bg-surface">
      <TabList className="bg-surface-raised">
        <TabButton
          id="overview"
          label="Overview"
          active={activeTab === "overview"}
          onClick={() => setActiveTab("overview")}
        />
        <TabButton
          id="files"
          label="Files"
          active={activeTab === "files"}
          onClick={() => setActiveTab("files")}
        />
        <TabButton
          id="peers"
          label="Peers"
          active={activeTab === "peers"}
          onClick={() => setActiveTab("peers")}
        />
      </TabList>
      <div className="flex-1 overflow-auto">
        <DetailPaneContent torrentId={selectedId} activeTab={activeTab} />
      </div>
    </div>
  );
};

interface DetailPaneContentProps {
  torrentId: number;
  activeTab: TabId;
}

const DetailPaneContent: React.FC<DetailPaneContentProps> = ({
  torrentId,
  activeTab,
}) => {
  const API = useContext(APIContext);
  const [fetchDetails, setFetchDetails] = useState(false);

  // Get torrent and details from store
  const torrent = useTorrentStore((state) =>
    state.torrents?.find((t) => t.id === torrentId),
  );
  const cachedDetails = useTorrentStore((state) => state.getDetails(torrentId));
  const setDetails = useTorrentStore((state) => state.setDetails);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  // Trigger fetch when Files tab is active and not cached
  useEffect(() => {
    if (activeTab !== "files") return;
    if (cachedDetails) return;
    setFetchDetails(true);
  }, [activeTab, torrentId, cachedDetails]);

  // Fetch details when requested
  useEffect(() => {
    if (!fetchDetails) return;
    return loopUntilSuccess(async () => {
      const details = await API.getTorrentDetails(torrentId);
      setDetails(torrentId, details);
      setFetchDetails(false);
    }, 1000);
  }, [fetchDetails, torrentId]);

  const forceRefreshCallback = () => {
    refreshTorrents();
    setFetchDetails(true);
  };

  const statsResponse = torrent?.stats ?? null;

  return (
    <>
      {activeTab === "overview" && <OverviewTab torrent={torrent ?? null} />}
      {activeTab === "files" && (
        <FilesTab
          torrentId={torrentId}
          detailsResponse={cachedDetails}
          statsResponse={statsResponse}
          onRefresh={forceRefreshCallback}
        />
      )}
      {activeTab === "peers" && <PeersTab torrent={torrent ?? null} />}
    </>
  );
};
