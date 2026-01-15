import { useContext, useEffect, useState } from "react";
import { TorrentDetails } from "../../api-types";
import { APIContext } from "../../context";
import { loopUntilSuccess } from "../../helper/loopUntilSuccess";
import { useUIStore } from "../../stores/uiStore";
import { useTorrentStore } from "../../stores/torrentStore";
import { OverviewTab } from "./OverviewTab";
import { FilesTab } from "./FilesTab";
import { PeersTab } from "./PeersTab";

type TabId = "overview" | "files" | "peers";

interface TabButtonProps {
  id: TabId;
  label: string;
  active: boolean;
  onClick: () => void;
}

const TabButton: React.FC<TabButtonProps> = ({
  id,
  label,
  active,
  onClick,
}) => (
  <button
    onClick={onClick}
    className={`px-4 py-2 font-medium border-b-2 transition-colors ${
      active
        ? "border-primary text-primary"
        : "border-transparent text-text-secondary hover:text-text hover:border-border"
    }`}
  >
    {label}
  </button>
);

export const DetailPane: React.FC = () => {
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);
  const [activeTab, setActiveTab] = useState<TabId>("overview");

  const selectedArray = Array.from(selectedTorrentIds);
  const selectedCount = selectedArray.length;

  if (selectedCount === 0) {
    return (
      <div className="h-full border-t border-border bg-surface-raised flex items-center justify-center">
        <p className="text-text-tertiary">Select a torrent to view details</p>
      </div>
    );
  }

  if (selectedCount > 1) {
    return (
      <div className="h-full border-t border-border bg-surface-raised flex items-center justify-center">
        <p className="text-text-tertiary">{selectedCount} torrents selected</p>
      </div>
    );
  }

  const selectedId = selectedArray[0];

  return (
    <div className="h-full border-t border-border flex flex-col bg-surface">
      <div className="flex border-b border-border bg-surface-raised">
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
      </div>
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
  // Cache of full details (with files) by torrent ID
  const [detailsCache, setDetailsCache] = useState<Map<number, TorrentDetails>>(
    new Map(),
  );
  const [fetchDetails, setFetchDetails] = useState(false);
  const API = useContext(APIContext);

  // Get torrent data from the store
  const torrent = useTorrentStore((state) =>
    state.torrents?.find((t) => t.id === torrentId),
  );
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  // Cached full details for current torrent (null if not fetched yet)
  const cachedDetails = detailsCache.get(torrentId) ?? null;

  const forceRefreshCallback = () => {
    refreshTorrents();
    setFetchDetails(true);
  };

  // Fetch full details (with files) only when Files tab is active and we don't have cached details
  useEffect(() => {
    if (activeTab !== "files") return;
    if (cachedDetails) return;
    setFetchDetails(true);
  }, [activeTab, torrentId, cachedDetails]);

  // Fetch full details when requested
  useEffect(() => {
    if (!fetchDetails) return;
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrentId).then((details) => {
        setDetailsCache((prev) => new Map(prev).set(torrentId, details));
        setFetchDetails(false);
      });
    }, 1000);
  }, [fetchDetails, torrentId]);

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
      {activeTab === "peers" && (
        <PeersTab torrentId={torrentId} statsResponse={statsResponse} />
      )}
    </>
  );
};
