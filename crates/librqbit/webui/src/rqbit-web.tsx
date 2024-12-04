import { useContext, useEffect, useState } from "react";
import { ErrorDetails as ApiErrorDetails } from "./api-types";
import { APIContext } from "./context";
import { RootContent } from "./components/RootContent";
import { customSetInterval } from "./helper/customSetInterval";
import { IconButton } from "./components/buttons/IconButton";
import { BsBodyText, BsMoon } from "react-icons/bs";
import { LogStreamModal } from "./components/modal/LogStreamModal";
import { Header } from "./components/Header";
import { DarkMode } from "./helper/darkMode";
import { useTorrentStore } from "./stores/torrentStore";
import { useErrorStore } from "./stores/errorStore";
import { AlertModal } from "./components/modal/AlertModal";
import { useStatsStore } from "./stores/statsStore";
import { Footer } from "./components/Footer";

export interface ErrorWithLabel {
  text: string;
  details?: ApiErrorDetails;
}

export interface ContextType {
  setCloseableError: (error: ErrorWithLabel | null) => void;
  refreshTorrents: () => void;
}

const urlSearchParams: URLSearchParams = new URLSearchParams(
  window.location.search,
);

const skipExtraWebuiButtons =
  urlSearchParams.get("skipExtraWebuiButtons") == "1";

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
    (state) => state.setTorrentsLoading
  );
  const setRefreshTorrents = useTorrentStore(
    (state) => state.setRefreshTorrents
  );

  const refreshTorrents = async () => {
    setTorrentsLoading(true);
    let torrents = await API.listTorrents().finally(() =>
      setTorrentsLoading(false)
    );
    setTorrents(torrents.torrents);
  };
  setRefreshTorrents(refreshTorrents);

  const setStats = useStatsStore((state) => state.setStats);

  useEffect(() => {
    return customSetInterval(
      async () =>
        refreshTorrents().then(
          () => {
            setOtherError(null);
            return 5000;
          },
          (e) => {
            setOtherError({ text: "Error refreshing torrents", details: e });
            console.error(e);
            return 5000;
          }
        ),
      0
    );
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
          }
        ),
      0
    );
  }, []);

  return (
    <div className="dark:bg-gray-900 dark:text-gray-200 min-h-screen flex flex-col">
      <Header title={props.title} version={props.version} />
      <div className="relative">
        {/* Menu buttons */}
        <div className="absolute top-0 start-0 pl-2 z-10">
          {props.menuButtons &&
            props.menuButtons.map((b, i) => <span key={i}>{b}</span>)}
          {!skipExtraWebuiButtons && (
            <>
              <IconButton onClick={() => setLogsOpened(true)}>
                <BsBodyText />
              </IconButton>
              <IconButton onClick={DarkMode.toggle}>
                <BsMoon />
              </IconButton>
            </>
          )}
        </div>
      </div>

      <div className="grow">
        <RootContent />
      </div>

      <Footer />

      <LogStreamModal show={logsOpened} onClose={() => setLogsOpened(false)} />
      <AlertModal />
    </div>
  );
};
