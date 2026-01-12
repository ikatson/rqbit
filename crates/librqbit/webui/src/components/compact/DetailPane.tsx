import { useContext, useEffect, useState } from "react";
import { TorrentDetails } from "../../api-types";
import { APIContext, RefreshTorrentStatsContext } from "../../context";
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

const TabButton: React.FC<TabButtonProps> = ({ id, label, active, onClick }) => (
  <button
    onClick={onClick}
    className={`
      px-4 py-2 text-sm font-medium border-b-2 transition-colors
      ${active
        ? "border-blue-500 text-blue-600 dark:text-blue-400"
        : "border-transparent text-gray-500 dark:text-slate-400 hover:text-gray-700 dark:hover:text-slate-300 hover:border-gray-300"
      }
    `}
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
      <div className="h-full border-t border-gray-200 dark:border-slate-700 bg-gray-50 dark:bg-slate-800/50 flex items-center justify-center">
        <p className="text-gray-400 dark:text-slate-500">Select a torrent to view details</p>
      </div>
    );
  }

  if (selectedCount > 1) {
    return (
      <div className="h-full border-t border-gray-200 dark:border-slate-700 bg-gray-50 dark:bg-slate-800/50 flex items-center justify-center">
        <p className="text-gray-400 dark:text-slate-500">{selectedCount} torrents selected</p>
      </div>
    );
  }

  const selectedId = selectedArray[0];

  return (
    <div className="h-full border-t border-gray-200 dark:border-slate-700 flex flex-col bg-white dark:bg-slate-900">
      <div className="flex border-b border-gray-200 dark:border-slate-700 bg-gray-50 dark:bg-slate-800">
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

const DetailPaneContent: React.FC<DetailPaneContentProps> = ({ torrentId, activeTab }) => {
  const [detailsResponse, setDetailsResponse] = useState<TorrentDetails | null>(null);
  const [fetchDetails, setFetchDetails] = useState(true);
  const API = useContext(APIContext);

  // Get torrent data from the store
  const torrent = useTorrentStore((state) =>
    state.torrents?.find((t) => t.id === torrentId)
  );
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  const forceRefreshCallback = () => {
    refreshTorrents();
    setFetchDetails(true);
  };

  // Reset details when torrent changes
  useEffect(() => {
    setDetailsResponse(null);
    setFetchDetails(true);
  }, [torrentId]);

  // Fetch full details (with files) when needed
  useEffect(() => {
    if (!fetchDetails) return;
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrentId).then((details) => {
        setDetailsResponse(details);
        setFetchDetails(false);
      });
    }, 1000);
  }, [fetchDetails, torrentId]);

  // Use stats from store, fall back to details response if store doesn't have it yet
  const statsResponse = torrent?.stats ?? null;

  return (
    <RefreshTorrentStatsContext.Provider value={{ refresh: forceRefreshCallback }}>
      {activeTab === "overview" && (
        <OverviewTab
          torrentId={torrentId}
          detailsResponse={detailsResponse}
          statsResponse={statsResponse}
        />
      )}
      {activeTab === "files" && (
        <FilesTab
          torrentId={torrentId}
          detailsResponse={detailsResponse}
          statsResponse={statsResponse}
        />
      )}
      {activeTab === "peers" && (
        <PeersTab
          torrentId={torrentId}
          statsResponse={statsResponse}
        />
      )}
    </RefreshTorrentStatsContext.Provider>
  );
};
