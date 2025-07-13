import { useContext } from "react";
import { TorrentsList } from "./TorrentsList";
import { ErrorComponent } from "./ErrorComponent";
import { useTorrentStore } from "../stores/torrentStore";
import { useErrorStore } from "../stores/errorStore";
import { ViewModeContext } from "../stores/viewMode";
import { TorrentDetailsPane } from "./TorrentDetailsPane";
import { APIContext } from "../context";
import { ResizablePanes } from "./ResizablePanes";

export const RootContent = (props: {}) => {
  const { compact } = useContext(ViewModeContext);

  let closeableError = useErrorStore((state) => state.closeableError);
  let setCloseableError = useErrorStore((state) => state.setCloseableError);
  let otherError = useErrorStore((state) => state.otherError);
  let selectedTorrentId = useTorrentStore((state) => state.selectedTorrentId);
  let torrents = useTorrentStore((state) => state.torrents);
  let selectedTorrent =
    torrents?.find((t) => t.id === selectedTorrentId) ?? null;
  let torrentsInitiallyLoading = useTorrentStore(
    (state) => state.torrentsInitiallyLoading,
  );

  const API = useContext(APIContext);

  if (compact) {
    return (
      <ResizablePanes
        top={
          <TorrentsList
            torrents={torrents}
            loading={torrentsInitiallyLoading}
          />
        }
        bottom={
          selectedTorrent !== null && (
            <TorrentDetailsPane torrent={selectedTorrent} />
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
