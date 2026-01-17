import { JSX, useContext, useEffect, useState } from "react";
import { ErrorDetails as ApiErrorDetails } from "./api-types";
import { APIContext } from "./context";
import { RootContent } from "./components/RootContent";
import { customSetInterval } from "./helper/customSetInterval";
import { LogStreamModal } from "./components/modal/LogStreamModal";
import { Header } from "./components/Header";
import { useTorrentStore } from "./stores/torrentStore";
import { useErrorStore } from "./stores/errorStore";
import { AlertModal } from "./components/modal/AlertModal";
import { useStatsStore } from "./stores/statsStore";
import { Footer } from "./components/Footer";
import { SettingsButtons } from "./components/SettingsButtons";

export interface ErrorWithLabel {
  text: string;
  details?: ApiErrorDetails;
}

export interface ContextType {
  setCloseableError: (error: ErrorWithLabel | null) => void;
  refreshTorrents: () => void;
}

export const RqbitWebUI = (props: {
  title: string;
  version: string;
  menuButtons?: JSX.Element[];
}) => {
  let [logsOpened, setLogsOpened] = useState<boolean>(false);
  const setOtherError = useErrorStore((state) => state.setOtherError);

  const API = useContext(APIContext);

  const setTorrents = useTorrentStore((state) => state.setTorrents);
  const setTorrentsLoading = useTorrentStore(
    (state) => state.setTorrentsLoading,
  );
  const setRefreshTorrents = useTorrentStore(
    (state) => state.setRefreshTorrents,
  );

  const refreshTorrents = async (): Promise<number> => {
    setTorrentsLoading(true);
    try {
      const response = await API.listTorrents({ withStats: true });
      setTorrents(response.torrents);
      setOtherError(null);

      // Determine polling interval based on torrent states
      // Fast poll (1s) if any torrent is live/initializing, slow poll (5s) otherwise
      const hasActiveTorrents = response.torrents.some(
        (t) => t.stats?.state === "live" || t.stats?.state === "initializing",
      );
      return hasActiveTorrents ? 1000 : 5000;
    } catch (e) {
      setOtherError({ text: "Error refreshing torrents", details: e as any });
      console.error(e);
      return 5000;
    } finally {
      setTorrentsLoading(false);
    }
  };

  const setStats = useStatsStore((state) => state.setStats);

  // Register the refresh callback
  useEffect(() => {
    setRefreshTorrents(refreshTorrents as unknown as () => void);
  }, []);

  useEffect(() => {
    return customSetInterval(async () => refreshTorrents(), 0);
  }, []);

  useEffect(() => {
    return customSetInterval(
      async () =>
        API.stats().then(
          (stats) => {
            setStats(stats);
            return 1000;
          },
          (e) => {
            console.error(e);
            return 5000;
          },
        ),
      0,
    );
  }, []);

  return (
    <div className="bg-surface h-dvh flex flex-col overflow-hidden">
      <Header
        title={props.title}
        version={props.version}
        settingsSlot={
          <SettingsButtons
            onLogsClick={() => setLogsOpened(true)}
            menuButtons={props.menuButtons}
          />
        }
      />

      <div className="grow min-h-0">
        <RootContent />
      </div>

      <Footer />

      <LogStreamModal show={logsOpened} onClose={() => setLogsOpened(false)} />
      <AlertModal />
    </div>
  );
};
