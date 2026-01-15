import { useState, useCallback, useMemo } from "react";
import { GoSearch, GoX } from "react-icons/go";
import debounce from "lodash.debounce";
import { TorrentListItem } from "../api-types";
import { Spinner } from "./Spinner";
import { TorrentCard } from "./TorrentCard";
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

const SEARCH_THRESHOLD = 10;
const CARD_SORT_STORAGE_KEY = "rqbit-card-sort";

function getDefaultCardSort(): { column: TorrentSortColumn; direction: SortDirection } {
  try {
    const stored = localStorage.getItem(CARD_SORT_STORAGE_KEY);
    if (stored) {
      const parsed = JSON.parse(stored);
      if (parsed.column && parsed.direction) {
        return parsed;
      }
    }
  } catch {
    // ignore
  }
  return { column: "id", direction: "desc" };
}

export const CardLayout = (props: {
  torrents: Array<TorrentListItem> | null;
  loading: boolean;
}) => {
  const searchQuery = useUIStore((state) => state.searchQuery);
  const setSearchQuery = useUIStore((state) => state.setSearchQuery);
  const statusFilter = useUIStore((state) => state.statusFilter);
  const setStatusFilter = useUIStore((state) => state.setStatusFilter);

  const [localSearch, setLocalSearch] = useState(searchQuery);
  const [sortColumn, setSortColumn] = useState<TorrentSortColumn>(
    () => getDefaultCardSort().column
  );
  const [sortDirection, setSortDirection] = useState<SortDirection>(
    () => getDefaultCardSort().direction
  );

  // eslint-disable-next-line react-hooks/exhaustive-deps
  const debouncedSetSearch = useCallback(
    debounce((value: string) => setSearchQuery(value), 150),
    [setSearchQuery]
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
    const [col, dir] = e.target.value.split(":") as [TorrentSortColumn, SortDirection];
    setSortColumn(col);
    setSortDirection(dir);
    localStorage.setItem(CARD_SORT_STORAGE_KEY, JSON.stringify({ column: col, direction: dir }));
  };

  const normalizedQuery = searchQuery.toLowerCase().trim();
  const totalCount = props.torrents?.length ?? 0;
  const showFilters = totalCount > SEARCH_THRESHOLD;

  // Sort torrents (but don't filter - filtering is done via visibility)
  const sortedTorrents = useMemo(() => {
    if (!props.torrents) return null;
    return [...props.torrents].sort((a, b) =>
      compareTorrents(a, b, sortColumn, sortDirection)
    );
  }, [props.torrents, sortColumn, sortDirection]);

  return (
    <div className="flex flex-col gap-2 mx-2 pb-3 sm:px-7 mt-3">
      {showFilters && (
        <div className="flex flex-wrap items-center gap-2 w-full max-w-2xl mx-auto mb-2">
          {/* Search input */}
          <div className="relative flex-1 min-w-48">
            <GoSearch className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-tertiary" />
            <input
              type="text"
              value={localSearch}
              onChange={handleSearchChange}
              placeholder="Search torrents..."
              className="w-full pl-9 pr-9 py-2 text-sm bg-surface border border-divider rounded-lg focus:outline-none focus:border-primary placeholder:text-tertiary"
            />
            {localSearch && (
              <button
                onClick={clearSearch}
                className="absolute right-2 top-1/2 -translate-y-1/2 p-1 text-tertiary hover:text-secondary rounded"
              >
                <GoX className="w-4 h-4" />
              </button>
            )}
          </div>

          {/* Status filter */}
          <select
            value={statusFilter}
            onChange={(e) => setStatusFilter(e.target.value as StatusFilter)}
            className="py-2 px-3 text-sm bg-surface border border-divider rounded-lg focus:outline-none focus:border-primary"
          >
            {(Object.keys(STATUS_FILTER_LABELS) as StatusFilter[]).map((status) => (
              <option key={status} value={status}>
                {STATUS_FILTER_LABELS[status]}
              </option>
            ))}
          </select>

          {/* Sort dropdown */}
          <select
            value={`${sortColumn}:${sortDirection}`}
            onChange={handleSortChange}
            className="py-2 px-3 text-sm bg-surface border border-divider rounded-lg focus:outline-none focus:border-primary"
          >
            {(Object.keys(SORT_COLUMN_LABELS) as TorrentSortColumn[]).flatMap((col) => [
              <option key={`${col}:desc`} value={`${col}:desc`}>
                {SORT_COLUMN_LABELS[col]} ↓
              </option>,
              <option key={`${col}:asc`} value={`${col}:asc`}>
                {SORT_COLUMN_LABELS[col]} ↑
              </option>,
            ])}
          </select>
        </div>
      )}
      {sortedTorrents === null ? (
        props.loading ? (
          <Spinner
            className="justify-center m-5"
            label="Loading torrent list"
          />
        ) : null
      ) : sortedTorrents.length === 0 ? (
        <p className="text-center">No existing torrents found.</p>
      ) : (
        sortedTorrents.map((t: TorrentListItem) => (
          <TorrentCard
            key={t.id}
            torrent={t}
            hidden={!isTorrentVisible(t, normalizedQuery, statusFilter)}
          />
        ))
      )}
    </div>
  );
};
