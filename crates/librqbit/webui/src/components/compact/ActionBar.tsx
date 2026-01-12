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

  // Helper to get torrent data by id
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

  const pauseSelected = async () => {
    setDisabled(true);
    try {
      for (const id of selectedTorrentIds) {
        // Skip already paused torrents
        const torrent = getTorrentById(id);
        if (torrent?.stats?.state === STATE_PAUSED) {
          continue;
        }
        try {
          await API.pause(id);
          // Refresh immediately after each operation
          refreshTorrents();
        } catch (e) {
          setCloseableError({
            text: `Error pausing torrent id=${id}`,
            details: e as ErrorDetails,
          });
        }
      }
    } finally {
      setDisabled(false);
    }
  };

  const resumeSelected = async () => {
    setDisabled(true);
    try {
      for (const id of selectedTorrentIds) {
        // Skip already live torrents
        const torrent = getTorrentById(id);
        if (torrent?.stats?.state === STATE_LIVE) {
          continue;
        }
        try {
          await API.start(id);
          // Refresh immediately after each operation
          refreshTorrents();
        } catch (e) {
          setCloseableError({
            text: `Error starting torrent id=${id}`,
            details: e as ErrorDetails,
          });
        }
      }
    } finally {
      setDisabled(false);
    }
  };

  const handleDeleteClick = () => {
    setShowDeleteModal(true);
  };

  const handleDeleteModalHide = () => {
    setShowDeleteModal(false);
  };

  return (
    <div className="flex items-center gap-2 px-4 py-2 bg-gray-50 dark:bg-slate-800 border-b border-gray-200 dark:border-slate-700">
      <button
        onClick={resumeSelected}
        disabled={disabled || !hasSelection}
        className="inline-flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium rounded-md
          bg-white dark:bg-slate-700 border border-gray-300 dark:border-slate-600
          text-gray-700 dark:text-slate-200
          hover:bg-gray-50 dark:hover:bg-slate-600
          disabled:opacity-50 disabled:cursor-not-allowed
          transition-colors"
        title="Resume selected torrents"
      >
        <FaPlay className="w-3 h-3" />
        Resume
      </button>
      <button
        onClick={pauseSelected}
        disabled={disabled || !hasSelection}
        className="inline-flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium rounded-md
          bg-white dark:bg-slate-700 border border-gray-300 dark:border-slate-600
          text-gray-700 dark:text-slate-200
          hover:bg-gray-50 dark:hover:bg-slate-600
          disabled:opacity-50 disabled:cursor-not-allowed
          transition-colors"
        title="Pause selected torrents"
      >
        <FaPause className="w-3 h-3" />
        Pause
      </button>
      <button
        onClick={handleDeleteClick}
        disabled={disabled || !hasSelection}
        className="inline-flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium rounded-md
          bg-white dark:bg-slate-700 border border-gray-300 dark:border-slate-600
          text-red-600 dark:text-red-400
          hover:bg-red-50 dark:hover:bg-red-900/20
          disabled:opacity-50 disabled:cursor-not-allowed
          transition-colors"
        title="Delete selected torrents"
      >
        <FaTrash className="w-3 h-3" />
        Delete
      </button>

      {hasSelection && (
        <span className="ml-2 text-sm text-gray-500 dark:text-slate-400">
          {selectedCount} selected
        </span>
      )}

      <DeleteTorrentModal
        show={showDeleteModal}
        onHide={handleDeleteModalHide}
        torrents={selectedTorrents}
      />
    </div>
  );
};
