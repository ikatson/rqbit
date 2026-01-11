import { create } from "zustand";
import { TorrentDetails, TorrentStats } from "../api-types";

const STORAGE_KEY = "rqbit-view-mode";
const SORT_STORAGE_KEY = "rqbit-torrent-sort";
const LARGE_SCREEN_BREAKPOINT = 1024;

function getDefaultViewMode(): "full" | "compact" {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored === "full" || stored === "compact") {
    return stored;
  }
  return window.innerWidth >= LARGE_SCREEN_BREAKPOINT ? "compact" : "full";
}

export type TorrentSortColumn = "id" | "name" | "progress" | "downSpeed" | "upSpeed" | "eta" | "peers";
export type SortDirection = "asc" | "desc";

interface StoredSort {
  column: TorrentSortColumn;
  direction: SortDirection;
}

function getDefaultSort(): StoredSort {
  try {
    const stored = localStorage.getItem(SORT_STORAGE_KEY);
    if (stored) {
      const parsed = JSON.parse(stored) as StoredSort;
      if (parsed.column && parsed.direction) {
        return parsed;
      }
    }
  } catch {
    // ignore
  }
  return { column: "id", direction: "desc" };
}

export interface TorrentDataCache {
  details: TorrentDetails | null;
  stats: TorrentStats | null;
}

export interface UIStore {
  viewMode: "full" | "compact";
  setViewMode: (mode: "full" | "compact") => void;
  toggleViewMode: () => void;

  selectedTorrentIds: Set<number>;
  selectTorrent: (id: number) => void;
  toggleSelection: (id: number) => void;
  deselectTorrent: (id: number) => void;
  clearSelection: () => void;
  selectAll: (ids: number[]) => void;

  // Sorting state
  sortColumn: TorrentSortColumn;
  sortDirection: SortDirection;
  setSortColumn: (column: TorrentSortColumn) => void;

  // Torrent data cache for sorting
  torrentDataCache: Map<number, TorrentDataCache>;
  updateTorrentDataCache: (id: number, data: Partial<TorrentDataCache>) => void;
  clearTorrentDataCache: (id: number) => void;
}

const defaultSort = getDefaultSort();

export const useUIStore = create<UIStore>((set, get) => ({
  viewMode: getDefaultViewMode(),

  setViewMode: (mode) => {
    localStorage.setItem(STORAGE_KEY, mode);
    set({ viewMode: mode });
  },

  toggleViewMode: () => {
    const newMode = get().viewMode === "compact" ? "full" : "compact";
    localStorage.setItem(STORAGE_KEY, newMode);
    set({ viewMode: newMode });
  },

  selectedTorrentIds: new Set<number>(),

  selectTorrent: (id) => {
    set({ selectedTorrentIds: new Set([id]) });
  },

  toggleSelection: (id) => {
    const current = get().selectedTorrentIds;
    const next = new Set(current);
    if (next.has(id)) {
      next.delete(id);
    } else {
      next.add(id);
    }
    set({ selectedTorrentIds: next });
  },

  deselectTorrent: (id) => {
    const current = get().selectedTorrentIds;
    if (current.has(id)) {
      const next = new Set(current);
      next.delete(id);
      set({ selectedTorrentIds: next });
    }
  },

  clearSelection: () => {
    set({ selectedTorrentIds: new Set() });
  },

  selectAll: (ids) => {
    set({ selectedTorrentIds: new Set(ids) });
  },

  // Sorting state
  sortColumn: defaultSort.column,
  sortDirection: defaultSort.direction,

  setSortColumn: (column) => {
    const current = get();
    let newDirection: SortDirection = "desc";
    if (current.sortColumn === column) {
      // Toggle direction if same column
      newDirection = current.sortDirection === "asc" ? "desc" : "asc";
    }
    localStorage.setItem(SORT_STORAGE_KEY, JSON.stringify({ column, direction: newDirection }));
    set({ sortColumn: column, sortDirection: newDirection });
  },

  // Torrent data cache
  torrentDataCache: new Map(),

  updateTorrentDataCache: (id, data) => {
    const cache = get().torrentDataCache;
    const existing = cache.get(id) ?? { details: null, stats: null };
    const updated = new Map(cache);
    updated.set(id, { ...existing, ...data });
    set({ torrentDataCache: updated });
  },

  clearTorrentDataCache: (id) => {
    const cache = get().torrentDataCache;
    if (cache.has(id)) {
      const updated = new Map(cache);
      updated.delete(id);
      set({ torrentDataCache: updated });
    }
  },
}));
