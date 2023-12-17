import { TorrentsList } from "./TorrentsList";
import { ErrorComponent } from "./ErrorComponent";
import { useTorrentStore } from "../stores/torrentStore";
import { useErrorStore } from "../stores/errorStore";

export const RootContent = (props: {}) => {
  let closeableError = useErrorStore((state) => state.closeableError);
  let setCloseableError = useErrorStore((state) => state.setCloseableError);
  let otherError = useErrorStore((state) => state.otherError);
  let torrents = useTorrentStore((state) => state.torrents);
  let torrentsLoading = useTorrentStore((state) => state.torrentsLoading);

  return (
    <div className="container mx-auto">
      <ErrorComponent
        error={closeableError}
        remove={() => setCloseableError(null)}
      />
      <ErrorComponent error={otherError} />
      <TorrentsList torrents={torrents} loading={torrentsLoading} />
    </div>
  );
};
