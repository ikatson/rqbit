import { useContext, useState, useEffect } from "react";
import { TorrentsList } from "./TorrentsList";
import { ErrorComponent } from "./ErrorComponent";
import { useTorrentStore } from "../stores/torrentStore";
import { useErrorStore } from "../stores/errorStore";
import { ViewModeContext } from "../stores/viewMode";
import { CompactTorrentsList } from "./CompactTorrentsList";
import { TorrentDetailsPane } from "./TorrentDetailsPane";
import { TorrentDetails, TorrentStats } from "../api-types";
import { APIContext } from "../context";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";

export const RootContent = (props: {}) => {
  const { compact } = useContext(ViewModeContext);
  const [selectedTorrent, setSelectedTorrent] = useState<number | null>(null);
  const [selectedTorrentDetails, setSelectedTorrentDetails] =
    useState<TorrentDetails | null>(null);

  let closeableError = useErrorStore((state) => state.closeableError);
  let setCloseableError = useErrorStore((state) => state.setCloseableError);
  let otherError = useErrorStore((state) => state.otherError);

  let torrents = useTorrentStore((state) => state.torrents);
  let torrentsInitiallyLoading = useTorrentStore(
    (state) => state.torrentsInitiallyLoading,
  );

  let selectedTorrentData =
    torrents?.find((t) => (t.id === selectedTorrent ? t : null)) ?? null;

  const API = useContext(APIContext);

  useEffect(() => {
    if (selectedTorrent === null) {
      setSelectedTorrentDetails(null);
      return;
    }
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(selectedTorrent).then(
        setSelectedTorrentDetails,
      );
    }, 1000);
  }, [selectedTorrent]);

  const onTorrentClick = (id: number) => {
    setSelectedTorrent(id);
  };

  if (compact) {
    return (
      <div className="flex flex-col h-full">
        <div className="h-1/2 overflow-y-auto">
          <CompactTorrentsList
            torrents={torrents}
            loading={torrentsInitiallyLoading}
            onTorrentClick={onTorrentClick}
            selectedTorrent={selectedTorrent}
          />
        </div>
        <div className="h-1/2 overflow-y-auto">
          {selectedTorrentData !== null && (
            <TorrentDetailsPane
              torrent={selectedTorrentData}
              details={selectedTorrentDetails}
            />
          )}
        </div>
      </div>
    );
  }

  return (
    <div className="container mx-auto">
      <ErrorComponent
        error={closeableError}
        remove={() => setCloseableError(null)}
      />
      <ErrorComponent error={otherError} />
      <TorrentsList torrents={torrents} loading={torrentsInitiallyLoading} />
    </div>
  );
};
