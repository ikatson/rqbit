import { useContext, useState } from "react";
import { generateMagnetLink } from "../../helper/magnetUtils";
import { TorrentListItem, TorrentStats } from "../../api-types";
import { APIContext } from "../../context";
import { IconButton } from "./IconButton";
import { DeleteTorrentModal } from "../modal/DeleteTorrentModal";
import { FaCog, FaMagnet, FaPause, FaPlay, FaTrash } from "react-icons/fa";
import { useErrorStore } from "../../stores/errorStore";
import { useTorrentStore } from "../../stores/torrentStore";
import { useUIStore } from "../../stores/uiStore";

export const TorrentActions: React.FC<{
  torrent: TorrentListItem & { stats: TorrentStats };
}> = ({ torrent }) => {
  const { id, name, stats } = torrent;
  const state = stats.state;

  let [disabled, setDisabled] = useState<boolean>(false);
  let [deleting, setDeleting] = useState<boolean>(false);

  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const openDetailsModal = useUIStore((state) => state.openDetailsModal);

  const canPause = state == "live";
  const canUnpause = state == "paused" || state == "error";
  const canConfigure = state == "paused" || state == "live";

  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const API = useContext(APIContext);

  const unpause = () => {
    setDisabled(true);
    API.start(id)
      .then(
        () => {
          refreshTorrents();
        },
        (e) => {
          setCloseableError({
            text: `Error starting torrent id=${id}`,
            details: e,
          });
        },
      )
      .finally(() => setDisabled(false));
  };

  const pause = () => {
    setDisabled(true);
    API.pause(id)
      .then(
        () => {
          refreshTorrents();
        },
        (e) => {
          setCloseableError({
            text: `Error pausing torrent id=${id}`,
            details: e,
          });
        },
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


  const copyMagnetLink = async () => {
    try {
        setDisabled(true);
        let trackers: string[] = [];
        try {
            const details = await API.getTorrentDetails(id);
            if (details.trackers && details.trackers.length > 0) {
                trackers = details.trackers;
            }
        } catch (e) {
            console.warn("Could not fetch details for magnet, using basic link", e);
        }

        const magnet = generateMagnetLink(torrent.info_hash, name || "", trackers);

        if (navigator.clipboard) {
            await navigator.clipboard.writeText(magnet);
        } else {
            // Fallback for non-secure contexts (though Tauri should be secure)
            const textArea = document.createElement("textarea");
            textArea.value = magnet;
            document.body.appendChild(textArea);
            textArea.select();
            document.execCommand("copy");
            document.body.removeChild(textArea);
        }
    } catch (e) {
      console.error("Failed to copy magnet link", e);
      setCloseableError({
          text: "Failed to copy magnet link",
          details: e as any
      });
    } finally {
        setDisabled(false);
    }
  };

  return (
    <div className="flex w-full justify-center gap-1 sm:gap-2 dark:text-slate-300">
      <IconButton onClick={copyMagnetLink} disabled={disabled} title="Copy Magnet Link">
        <FaMagnet className="hover:text-blue-500" />
      </IconButton>
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
      {canConfigure && (
        <IconButton onClick={() => openDetailsModal(id)} disabled={disabled}>
          <FaCog className="hover:text-green-600" />
        </IconButton>
      )}
      <IconButton onClick={startDeleting} disabled={disabled}>
        <FaTrash className="hover:text-red-500" />
      </IconButton>
      <DeleteTorrentModal
        show={deleting}
        onHide={cancelDeleting}
        torrents={[{ id, name }]}
      />
    </div>
  );
};
