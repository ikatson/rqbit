import { useContext, useState, useEffect } from "react";
import { TorrentsList } from "./TorrentsList";
import { ErrorComponent } from "./ErrorComponent";
import { useTorrentStore } from "../stores/torrentStore";
import { useErrorStore } from "../stores/errorStore";
import { ViewModeContext } from "../stores/viewMode";
import { TorrentDetailsPane } from "./TorrentDetailsPane";
import { TorrentDetails, TorrentStats } from "../api-types";
import { APIContext } from "../context";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { ResizablePanes } from "./ResizablePanes";

export const RootContent = (props: {}) => {
  const { compact } = useContext(ViewModeContext);
  const selectedTorrent = useTorrentStore((state) => state.selectedTorrent);
  const setSelectedTorrent = useTorrentStore(
    (state) => state.setSelectedTorrent,
  );
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
    selectedTorrent ?? null;

  const API = useContext(APIContext);

  useEffect(() => {
    if (selectedTorrent === null) {
      setSelectedTorrentDetails(null);
      return;
    }
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(selectedTorrent.id).then(
        setSelectedTorrentDetails,
      );
    }, 1000);
  }, [selectedTorrent]);

  const onTorrentClick = (id: number) => {
    setSelectedTorrent(torrents?.find((t) => t.id === id) ?? null);
  };

  if (compact) {
    return (
      <ResizablePanes
        top={
          <TorrentsList
            torrents={torrents}
            loading={torrentsInitiallyLoading}
            onTorrentClick={onTorrentClick}
            
          />
        }
        bottom={
          selectedTorrentData !== null && (
            <TorrentDetailsPane
              torrent={selectedTorrentData}
              details={selectedTorrentDetails}
            />
          )
        }
      />
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
