import { CardLayout } from "./CardLayout";
import { ErrorComponent } from "./ErrorComponent";
import { useTorrentStore } from "../stores/torrentStore";
import { useErrorStore } from "../stores/errorStore";
import { useUIStore } from "../stores/uiStore";
import { useIsLargeScreen } from "../hooks/useIsLargeScreen";
import { CompactLayout } from "./compact/CompactLayout";

export const RootContent = (props: {}) => {
  let closeableError = useErrorStore((state) => state.closeableError);
  let setCloseableError = useErrorStore((state) => state.setCloseableError);
  let otherError = useErrorStore((state) => state.otherError);
  let torrents = useTorrentStore((state) => state.torrents);
  let torrentsInitiallyLoading = useTorrentStore(
    (state) => state.torrentsInitiallyLoading
  );

  const viewMode = useUIStore((state) => state.viewMode);
  const isLargeScreen = useIsLargeScreen();

  const useCompactLayout = viewMode === "compact" && isLargeScreen;

  return (
    <div className={useCompactLayout ? "" : "container mx-auto"}>
      <ErrorComponent
        error={closeableError}
        remove={() => setCloseableError(null)}
      />
      <ErrorComponent error={otherError} />
      {useCompactLayout ? (
        <CompactLayout torrents={torrents} loading={torrentsInitiallyLoading} />
      ) : (
        <CardLayout torrents={torrents} loading={torrentsInitiallyLoading} />
      )}
    </div>
  );
};
