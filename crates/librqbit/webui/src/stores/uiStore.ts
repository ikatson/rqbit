import { create } from "zustand";

const STORAGE_KEY = "rqbit-view-mode";
const LARGE_SCREEN_BREAKPOINT = 1024;

function getDefaultViewMode(): "full" | "compact" {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored === "full" || stored === "compact") {
    return stored;
  }
  return window.innerWidth >= LARGE_SCREEN_BREAKPOINT ? "compact" : "full";
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
}

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
}));
