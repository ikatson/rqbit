import { useContext, useMemo, useState } from "react";
import { FaPause, FaPlay, FaTrash } from "react-icons/fa";
import { APIContext } from "../../context";
import { useUIStore } from "../../stores/uiStore";
import { useTorrentStore } from "../../stores/torrentStore";
import { useErrorStore } from "../../stores/errorStore";
import {
  DeleteTorrentModal,
  TorrentToDelete,
} from "../modal/DeleteTorrentModal";
import { ErrorDetails, STATE_LIVE, STATE_PAUSED } from "../../api-types";

const btnBase =
  "inline-flex items-center gap-1 px-2 py-1 text-xs font-medium rounded border transition-colors disabled:opacity-50 disabled:cursor-not-allowed";
const btnDefault = `${btnBase} bg-white dark:bg-slate-700 border-gray-300 dark:border-slate-600 text-gray-700 dark:text-slate-200 hover:bg-gray-50 dark:hover:bg-slate-600`;
const btnDanger = `${btnBase} bg-white dark:bg-slate-700 border-gray-300 dark:border-slate-600 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-900/20`;

export const ActionBar: React.FC = () => {
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);
  const torrents = useTorrentStore((state) => state.torrents);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const [disabled, setDisabled] = useState(false);
  const [showDeleteModal, setShowDeleteModal] = useState(false);

  const API = useContext(APIContext);

  const selectedCount = selectedTorrentIds.size;
  const hasSelection = selectedCount > 0;

  const getTorrentById = (id: number) => torrents?.find((t) => t.id === id);

  const selectedTorrents = useMemo((): TorrentToDelete[] => {
    return Array.from(selectedTorrentIds).map((id) => {
      const torrent = getTorrentById(id);
      return {
        id,
        details: torrent
          ? {
              name: torrent.name,
              info_hash: torrent.info_hash,
              files: [],
              total_pieces: torrent.total_pieces,
            }
          : null,
      };
    });
  }, [selectedTorrentIds, torrents]);

  const runBulkAction = async (
    action: (id: number) => Promise<void>,
    skipState: string,
    errorLabel: string
  ) => {
    setDisabled(true);
    try {
      for (const id of selectedTorrentIds) {
        const torrent = getTorrentById(id);
        if (torrent?.stats?.state === skipState) continue;
        try {
          await action(id);
          refreshTorrents();
        } catch (e) {
          setCloseableError({
            text: `Error ${errorLabel} torrent id=${id}`,
            details: e as ErrorDetails,
          });
        }
      }
    } finally {
      setDisabled(false);
    }
  };

  const pauseSelected = () =>
    runBulkAction((id) => API.pause(id), STATE_PAUSED, "pausing");
  const resumeSelected = () =>
    runBulkAction((id) => API.start(id), STATE_LIVE, "starting");

  return (
    <div className="flex items-center gap-1.5 px-3 py-1.5 bg-gray-50 dark:bg-slate-800 border-b border-gray-200 dark:border-slate-700">
      <button
        onClick={resumeSelected}
        disabled={disabled || !hasSelection}
        className={btnDefault}
        title="Resume selected torrents"
      >
        <FaPlay className="w-2.5 h-2.5" />
        Resume
      </button>
      <button
        onClick={pauseSelected}
        disabled={disabled || !hasSelection}
        className={btnDefault}
        title="Pause selected torrents"
      >
        <FaPause className="w-2.5 h-2.5" />
        Pause
      </button>
      <button
        onClick={() => setShowDeleteModal(true)}
        disabled={disabled || !hasSelection}
        className={btnDanger}
        title="Delete selected torrents"
      >
        <FaTrash className="w-2.5 h-2.5" />
        Delete
      </button>

      {hasSelection && (
        <span className="ml-1.5 text-xs text-gray-500 dark:text-slate-400">
          {selectedCount} selected
        </span>
      )}

      <DeleteTorrentModal
        show={showDeleteModal}
        onHide={() => setShowDeleteModal(false)}
        torrents={selectedTorrents}
      />
    </div>
  );
};
