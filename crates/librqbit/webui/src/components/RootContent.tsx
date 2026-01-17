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
    (state) => state.torrentsInitiallyLoading,
  );

  const viewMode = useUIStore((state) => state.viewMode);
  const isLargeScreen = useIsLargeScreen();

  const useCompactLayout = viewMode === "compact" && isLargeScreen;

  return (
    <div className={useCompactLayout ? "h-full" : "h-full flex flex-col"}>
      <ErrorComponent
        error={closeableError}
        remove={() => setCloseableError(null)}
      />
      <ErrorComponent error={otherError} />
      {useCompactLayout ? (
        <CompactLayout torrents={torrents} loading={torrentsInitiallyLoading} />
      ) : (
        <div className="flex-1 min-h-0">
          <CardLayout torrents={torrents} loading={torrentsInitiallyLoading} />
        </div>
      )}
    </div>
  );
};
