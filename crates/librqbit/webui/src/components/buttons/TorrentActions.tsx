import { useContext, useState } from "react";
import { TorrentStats } from "../../api-types";
import { APIContext, RefreshTorrentStatsContext } from "../../context";
import { IconButton } from "./IconButton";
import { DeleteTorrentModal } from "../modal/DeleteTorrentModal";
import { FaPause, FaPlay, FaTrash } from "react-icons/fa";
import { useErrorStore } from "../../stores/errorStore";

export const TorrentActions: React.FC<{
  id: number;
  statsResponse: TorrentStats;
}> = ({ id, statsResponse }) => {
  let state = statsResponse.state;

  let [disabled, setDisabled] = useState<boolean>(false);
  let [deleting, setDeleting] = useState<boolean>(false);

  let refreshCtx = useContext(RefreshTorrentStatsContext);

  const canPause = state == "live";
  const canUnpause = state == "paused" || state == "error";

  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const API = useContext(APIContext);

  const unpause = () => {
    setDisabled(true);
    API.start(id)
      .then(
        () => {
          refreshCtx.refresh();
        },
        (e) => {
          setCloseableError({
            text: `Error starting torrent id=${id}`,
            details: e,
          });
        }
      )
      .finally(() => setDisabled(false));
  };

  const pause = () => {
    setDisabled(true);
    API.pause(id)
      .then(
        () => {
          refreshCtx.refresh();
        },
        (e) => {
          setCloseableError({
            text: `Error pausing torrent id=${id}`,
            details: e,
          });
        }
      )
      .finally(() => setDisabled(false));
  };

  const startDeleting = () => {
    setDisabled(true);
    setDeleting(true);
  };

  const cancelDeleting = () => {
    setDisabled(false);
    setDeleting(false);
  };

  return (
    <div className="flex w-full justify-center gap-2 dark:text-slate-300">
      {canUnpause && (
        <IconButton onClick={unpause} disabled={disabled}>
          <FaPlay className="hover:text-green-600" />
        </IconButton>
      )}
      {canPause && (
        <IconButton onClick={pause} disabled={disabled}>
          <FaPause className="hover:text-amber-500" />
        </IconButton>
      )}
      <IconButton onClick={startDeleting} disabled={disabled}>
        <FaTrash className="hover:text-red-500" />
      </IconButton>
      <DeleteTorrentModal id={id} show={deleting} onHide={cancelDeleting} />
    </div>
  );
};
