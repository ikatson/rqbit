import { useContext, useState } from "react";
import { FaPause, FaPlay, FaTrash } from "react-icons/fa";
import { APIContext } from "../../context";
import { useUIStore } from "../../stores/uiStore";
import { useTorrentStore } from "../../stores/torrentStore";
import { useErrorStore } from "../../stores/errorStore";
import { DeleteTorrentModal } from "../modal/DeleteTorrentModal";

export const ActionBar: React.FC = () => {
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);
  const clearSelection = useUIStore((state) => state.clearSelection);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const [disabled, setDisabled] = useState(false);
  const [showDeleteModal, setShowDeleteModal] = useState(false);

  const API = useContext(APIContext);

  const selectedCount = selectedTorrentIds.size;
  const hasSelection = selectedCount > 0;

  const pauseSelected = async () => {
    setDisabled(true);
    try {
      const promises = Array.from(selectedTorrentIds).map((id) =>
        API.pause(id).catch((e) => {
          setCloseableError({
            text: `Error pausing torrent id=${id}`,
            details: e,
          });
        })
      );
      await Promise.all(promises);
      refreshTorrents();
    } finally {
      setDisabled(false);
    }
  };

  const resumeSelected = async () => {
    setDisabled(true);
    try {
      const promises = Array.from(selectedTorrentIds).map((id) =>
        API.start(id).catch((e) => {
          setCloseableError({
            text: `Error starting torrent id=${id}`,
            details: e,
          });
        })
      );
      await Promise.all(promises);
      refreshTorrents();
    } finally {
      setDisabled(false);
    }
  };

  const handleDeleteClick = () => {
    setShowDeleteModal(true);
  };

  const handleDeleteConfirm = async (deleteFiles: boolean) => {
    setDisabled(true);
    setShowDeleteModal(false);
    try {
      const promises = Array.from(selectedTorrentIds).map((id) => {
        const deleteMethod = deleteFiles ? API.delete : API.forget;
        return deleteMethod(id).catch((e) => {
          setCloseableError({
            text: `Error deleting torrent id=${id}`,
            details: e,
          });
        });
      });
      await Promise.all(promises);
      clearSelection();
      refreshTorrents();
    } finally {
      setDisabled(false);
    }
  };

  const handleDeleteCancel = () => {
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

      {showDeleteModal && (
        <BulkDeleteModal
          count={selectedCount}
          onConfirm={handleDeleteConfirm}
          onCancel={handleDeleteCancel}
        />
      )}
    </div>
  );
};

interface BulkDeleteModalProps {
  count: number;
  onConfirm: (deleteFiles: boolean) => void;
  onCancel: () => void;
}

const BulkDeleteModal: React.FC<BulkDeleteModalProps> = ({
  count,
  onConfirm,
  onCancel,
}) => {
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="bg-white dark:bg-slate-800 rounded-lg shadow-xl p-6 max-w-md w-full mx-4">
        <h3 className="text-lg font-semibold text-gray-900 dark:text-slate-100 mb-4">
          Delete {count} torrent{count > 1 ? "s" : ""}?
        </h3>
        <p className="text-sm text-gray-600 dark:text-slate-400 mb-6">
          Choose whether to also delete the downloaded files.
        </p>
        <div className="flex flex-col gap-2">
          <button
            onClick={() => onConfirm(false)}
            className="w-full px-4 py-2 text-sm font-medium rounded-md
              bg-gray-100 dark:bg-slate-700
              text-gray-700 dark:text-slate-200
              hover:bg-gray-200 dark:hover:bg-slate-600
              transition-colors"
          >
            Remove from list only
          </button>
          <button
            onClick={() => onConfirm(true)}
            className="w-full px-4 py-2 text-sm font-medium rounded-md
              bg-red-600 text-white
              hover:bg-red-700
              transition-colors"
          >
            Delete with files
          </button>
          <button
            onClick={onCancel}
            className="w-full px-4 py-2 text-sm font-medium rounded-md
              text-gray-500 dark:text-slate-400
              hover:text-gray-700 dark:hover:text-slate-200
              transition-colors"
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
};
