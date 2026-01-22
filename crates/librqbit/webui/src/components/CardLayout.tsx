import { useState, useCallback, useMemo } from "react";
import { Virtuoso } from "react-virtuoso";
import { GoSearch, GoX } from "react-icons/go";
import debounce from "lodash.debounce";
import { TorrentListItem } from "../api-types";
import { Spinner } from "./Spinner";
import { TorrentCard } from "./TorrentCard";
import { TorrentDetailsModal } from "./modal/TorrentDetailsModal";
import { useUIStore } from "../stores/uiStore";
import {
  TorrentSortColumn,
  SortDirection,
  StatusFilter,
  SORT_COLUMN_LABELS,
  STATUS_FILTER_LABELS,
  compareTorrents,
  isTorrentVisible,
} from "../helper/torrentFilters";
import { useKeyboardShortcuts } from "../hooks/useKeyboardShortcuts";

const DEFAULT_SORT_COLUMN: TorrentSortColumn = "id";
const DEFAULT_SORT_DIRECTION: SortDirection = "desc";

export const CardLayout = (props: {
  torrents: Array<TorrentListItem> | null;
  loading: boolean;
}) => {
  const searchQuery = useUIStore((state) => state.searchQuery);
  const setSearchQuery = useUIStore((state) => state.setSearchQuery);
  const statusFilter = useUIStore((state) => state.statusFilter);
  const setStatusFilter = useUIStore((state) => state.setStatusFilter);

  // Keyboard shortcuts (Ctrl+A, Ctrl+F, Escape)
  useKeyboardShortcuts();
  const detailsModalTorrentId = useUIStore(
    (state) => state.detailsModalTorrentId,
  );
  const closeDetailsModal = useUIStore((state) => state.closeDetailsModal);

  const [localSearch, setLocalSearch] = useState(searchQuery);
  const [sortColumn, setSortColumn] =
    useState<TorrentSortColumn>(DEFAULT_SORT_COLUMN);
  const [sortDirection, setSortDirection] = useState<SortDirection>(
    DEFAULT_SORT_DIRECTION,
  );

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

  const handleSortChange = (e: React.ChangeEvent<HTMLSelectElement>) => {
    const [col, dir] = e.target.value.split(":") as [
      TorrentSortColumn,
      SortDirection,
    ];
    setSortColumn(col);
    setSortDirection(dir);
  };

  const normalizedQuery = searchQuery.toLowerCase().trim();

  // Sort and filter torrents for virtualization
  const filteredTorrents = useMemo(() => {
    if (!props.torrents) return null;
    return [...props.torrents]
      .filter((t) => isTorrentVisible(t, normalizedQuery, statusFilter))
      .sort((a, b) => compareTorrents(a, b, sortColumn, sortDirection));
  }, [
    props.torrents,
    normalizedQuery,
    statusFilter,
    sortColumn,
    sortDirection,
  ]);

  // Item renderer for react-virtuoso
  const itemContent = useCallback(
    (index: number) => {
      const torrent = filteredTorrents![index];
      return (
        <div className="pb-1.5 sm:pb-2 px-2 sm:px-7 max-w-4xl mx-auto w-full">
          <TorrentCard key={torrent.id} torrent={torrent} />
        </div>
      );
    },
    [filteredTorrents],
  );

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center gap-1.5 sm:gap-2 w-full max-w-2xl mx-auto mb-2 mt-3 px-2 sm:px-7">
        {/* Search input */}
        <div className="relative flex-1 min-w-0">
          <GoSearch className="absolute left-2.5 sm:left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-tertiary" />
          <input
            type="text"
            data-search-input
            value={localSearch}
            onChange={handleSearchChange}
            placeholder="Search..."
            className="w-full pl-8 sm:pl-9 pr-8 sm:pr-9 py-1.5 sm:py-2 text-sm bg-surface border border-divider rounded-lg focus:outline-none focus:border-primary placeholder:text-tertiary"
          />
          {localSearch && (
            <button
              onClick={clearSearch}
              className="absolute right-1.5 sm:right-2 top-1/2 -translate-y-1/2 p-1 text-tertiary hover:text-secondary rounded cursor-pointer"
            >
              <GoX className="w-4 h-4" />
            </button>
          )}
        </div>

        {/* Status filter */}
        <select
          value={statusFilter}
          onChange={(e) => setStatusFilter(e.target.value as StatusFilter)}
          className="py-1.5 sm:py-2 px-2 sm:px-3 text-sm bg-surface border border-divider rounded-lg focus:outline-none focus:border-primary"
        >
          {(Object.keys(STATUS_FILTER_LABELS) as StatusFilter[]).map(
            (status) => (
              <option key={status} value={status}>
                {STATUS_FILTER_LABELS[status]}
              </option>
            ),
          )}
        </select>

        {/* Sort dropdown */}
        <select
          value={`${sortColumn}:${sortDirection}`}
          onChange={handleSortChange}
          className="py-1.5 sm:py-2 px-2 sm:px-3 text-sm bg-surface border border-divider rounded-lg focus:outline-none focus:border-primary"
        >
          {(Object.keys(SORT_COLUMN_LABELS) as TorrentSortColumn[]).flatMap(
            (col) => [
              <option key={`${col}:desc`} value={`${col}:desc`}>
                {SORT_COLUMN_LABELS[col]} ↓
              </option>,
              <option key={`${col}:asc`} value={`${col}:asc`}>
                {SORT_COLUMN_LABELS[col]} ↑
              </option>,
            ],
          )}
        </select>
      </div>
      {filteredTorrents === null ? (
        props.loading ? (
          <Spinner
            className="justify-center m-5"
            label="Loading torrent list"
          />
        ) : null
      ) : filteredTorrents.length === 0 ? (
        <p className="text-center">No existing torrents found.</p>
      ) : (
        <div className="flex-1 min-h-0">
          <Virtuoso
            totalCount={filteredTorrents.length}
            itemContent={itemContent}
          />
        </div>
      )}
      {detailsModalTorrentId !== null && (
        <TorrentDetailsModal
          torrentId={detailsModalTorrentId}
          isOpen={true}
          onClose={closeDetailsModal}
        />
      )}
    </div>
  );
};
