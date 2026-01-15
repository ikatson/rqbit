import { useState, useCallback } from "react";
import { GoSearch, GoX } from "react-icons/go";
import debounce from "lodash.debounce";
import { TorrentListItem } from "../api-types";
import { Spinner } from "./Spinner";
import { TorrentCard } from "./TorrentCard";
import { useUIStore } from "../stores/uiStore";

const SEARCH_THRESHOLD = 10;

function matchesSearch(name: string | null, query: string): boolean {
  if (!query) return true;
  return (name ?? "").toLowerCase().includes(query);
}

export const CardLayout = (props: {
  torrents: Array<TorrentListItem> | null;
  loading: boolean;
}) => {
  const searchQuery = useUIStore((state) => state.searchQuery);
  const setSearchQuery = useUIStore((state) => state.setSearchQuery);
  const [localSearch, setLocalSearch] = useState(searchQuery);

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

  const normalizedQuery = searchQuery.toLowerCase().trim();
  const totalCount = props.torrents?.length ?? 0;
  const showSearch = totalCount > SEARCH_THRESHOLD;

  return (
    <div className="flex flex-col gap-2 mx-2 pb-3 sm:px-7 mt-3">
      {showSearch && (
        <div className="relative w-full max-w-md mx-auto mb-2">
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
      )}
      {props.torrents === null ? (
        props.loading ? (
          <Spinner
            className="justify-center m-5"
            label="Loading torrent list"
          />
        ) : null
      ) : props.torrents.length === 0 ? (
        <p className="text-center">No existing torrents found.</p>
      ) : (
        props.torrents.map((t: TorrentListItem) => (
          <TorrentCard
            key={t.id}
            torrent={t}
            hidden={!matchesSearch(t.name, normalizedQuery)}
          />
        ))
      )}
    </div>
  );
};
