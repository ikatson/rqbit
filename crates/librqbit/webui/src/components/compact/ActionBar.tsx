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
import { Button } from "../buttons/Button";

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
        name: torrent?.name ?? null,
      };
    });
  }, [selectedTorrentIds, torrents]);

  const runBulkAction = async (
    action: (id: number) => Promise<void>,
    skipState: string,
    errorLabel: string,
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
    <div className="flex items-center gap-1.5 px-3 py-1.5 bg-surface-raised border-b border-divider">
      <Button
        onClick={resumeSelected}
        disabled={disabled || !hasSelection}
        variant="secondary"
      >
        <FaPlay className="w-2.5 h-2.5" />
        Resume
      </Button>
      <Button
        onClick={pauseSelected}
        disabled={disabled || !hasSelection}
        variant="secondary"
      >
        <FaPause className="w-2.5 h-2.5" />
        Pause
      </Button>
      <Button
        onClick={() => setShowDeleteModal(true)}
        disabled={disabled || !hasSelection}
        variant="danger"
      >
        <FaTrash className="w-2.5 h-2.5" />
        Delete
      </Button>

      {hasSelection && (
        <span className="ml-1.5 text-secondary">
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
