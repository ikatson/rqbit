import { useContext, useState, useCallback } from "react";
import { FaPause, FaPlay, FaTrash } from "react-icons/fa";
import { GoSearch, GoX } from "react-icons/go";
import debounce from "lodash.debounce";
import { APIContext } from "../../context";
import { useUIStore } from "../../stores/uiStore";
import { useTorrentStore } from "../../stores/torrentStore";
import { useErrorStore } from "../../stores/errorStore";
import { DeleteTorrentModal } from "../modal/DeleteTorrentModal";
import {
  ErrorDetails,
  STATE_LIVE,
  STATE_PAUSED,
  TorrentListItem,
} from "../../api-types";
import { Button } from "../buttons/Button";
import {
  StatusFilter,
  STATUS_FILTER_LABELS,
} from "../../helper/torrentFilters";

interface ActionBarProps {
  // When true, hides search/filter/selection count (for use in modal)
  hideFilters?: boolean;
}

export const ActionBar: React.FC<ActionBarProps> = ({ hideFilters }) => {
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);
  const searchQuery = useUIStore((state) => state.searchQuery);
  const setSearchQuery = useUIStore((state) => state.setSearchQuery);
  const statusFilter = useUIStore((state) => state.statusFilter);
  const setStatusFilter = useUIStore((state) => state.setStatusFilter);
  const torrents = useTorrentStore((state) => state.torrents);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const [disabled, setDisabled] = useState(false);
  const [showDeleteModal, setShowDeleteModal] = useState(false);
  const [torrentsToDelete, setTorrentsToDelete] = useState<
    Pick<TorrentListItem, "id" | "name">[]
  >([]);
  const [localSearch, setLocalSearch] = useState(searchQuery);

  // Debounced update to store
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const debouncedSetSearch = useCallback(
    debounce((value: string) => setSearchQuery(value), 150),
    [setSearchQuery],
  );

  const handleSearchChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const value = e.target.value;
    setLocalSearch(value);
    debouncedSetSearch(value);
  };

  const clearSearch = () => {
    setLocalSearch("");
    setSearchQuery("");
  };

  const API = useContext(APIContext);

  const selectedCount = selectedTorrentIds.size;
  const hasSelection = selectedCount > 0;

  const getTorrentById = (id: number) => torrents?.find((t) => t.id === id);

  const openDeleteModal = () => {
    // Capture current selection when opening modal (stable snapshot)
    const torrents = Array.from(selectedTorrentIds).map((id) => {
      const torrent = getTorrentById(id);
      return {
        id,
        name: torrent?.name ?? null,
      };
    });
    setTorrentsToDelete(torrents);
    setShowDeleteModal(true);
  };

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
        onClick={openDeleteModal}
        disabled={disabled || !hasSelection}
        variant="danger"
      >
        <FaTrash className="w-2.5 h-2.5" />
        Delete
      </Button>

      {!hideFilters && (
        <>
          {hasSelection && (
            <span className="ml-1.5 text-secondary">
              {selectedCount} selected
            </span>
          )}

          {/* Spacer */}
          <div className="flex-1" />

          {/* Status filter */}
          <select
            value={statusFilter}
            onChange={(e) => setStatusFilter(e.target.value as StatusFilter)}
            className="py-1 px-2 text-sm bg-surface border border-divider rounded focus:outline-none focus:border-primary"
          >
            {(Object.keys(STATUS_FILTER_LABELS) as StatusFilter[]).map(
              (status) => (
                <option key={status} value={status}>
                  {STATUS_FILTER_LABELS[status]}
                </option>
              ),
            )}
          </select>

          {/* Search input */}
          <div className="relative">
            <GoSearch className="absolute left-2 top-1/2 -translate-y-1/2 w-3.5 h-3.5 text-tertiary" />
            <input
              type="text"
              value={localSearch}
              onChange={handleSearchChange}
              placeholder="Search..."
              className="pl-7 pr-7 py-1 w-48 text-sm bg-surface border border-divider rounded focus:outline-none focus:border-primary placeholder:text-tertiary"
            />
            {localSearch && (
              <button
                onClick={clearSearch}
                className="absolute right-1.5 top-1/2 -translate-y-1/2 p-0.5 text-tertiary hover:text-secondary rounded cursor-pointer"
              >
                <GoX className="w-3.5 h-3.5" />
              </button>
            )}
          </div>
        </>
      )}

      <DeleteTorrentModal
        show={showDeleteModal}
        onHide={() => setShowDeleteModal(false)}
        torrents={torrentsToDelete}
      />
    </div>
  );
};
