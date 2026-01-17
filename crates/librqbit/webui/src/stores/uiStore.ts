import { create } from "zustand";
import {
  TorrentSortColumn,
  SortDirection,
  StatusFilter,
} from "../helper/torrentFilters";

const LARGE_SCREEN_BREAKPOINT = 1024;

function getDefaultViewMode(): "full" | "compact" {
  return window.innerWidth >= LARGE_SCREEN_BREAKPOINT ? "compact" : "full";
}

export interface UIStore {
  viewMode: "full" | "compact";
  setViewMode: (mode: "full" | "compact") => void;
  toggleViewMode: () => void;

  searchQuery: string;
  setSearchQuery: (query: string) => void;

  statusFilter: StatusFilter;
  setStatusFilter: (filter: StatusFilter) => void;

  selectedTorrentIds: Set<number>;
  lastSelectedId: number | null;
  selectTorrent: (id: number) => void;
  toggleSelection: (id: number) => void;
  selectRange: (id: number, orderedIds: number[]) => void;
  deselectTorrent: (id: number) => void;
  clearSelection: () => void;
  selectAll: (ids: number[]) => void;
  selectRelative: (direction: "up" | "down", orderedIds: number[]) => void;

  detailsModalTorrentId: number | null;
  openDetailsModal: (id: number) => void;
  closeDetailsModal: () => void;
}

export const useUIStore = create<UIStore>((set, get) => ({
  viewMode: getDefaultViewMode(),

  setViewMode: (mode) => {
    set({ viewMode: mode });
  },

  toggleViewMode: () => {
    const newMode = get().viewMode === "compact" ? "full" : "compact";
    set({ viewMode: newMode });
  },

  searchQuery: "",
  setSearchQuery: (query) => set({ searchQuery: query }),

  statusFilter: "all",
  setStatusFilter: (filter) => {
    set({ statusFilter: filter });
  },

  selectedTorrentIds: new Set<number>(),
  lastSelectedId: null,

  selectTorrent: (id) => {
    set({ selectedTorrentIds: new Set([id]), lastSelectedId: id });
  },

  toggleSelection: (id) => {
    const current = get().selectedTorrentIds;
    const next = new Set(current);
    if (next.has(id)) {
      next.delete(id);
    } else {
      next.add(id);
    }
    set({ selectedTorrentIds: next, lastSelectedId: id });
  },

  selectRange: (id, orderedIds) => {
    const { lastSelectedId, selectedTorrentIds } = get();
    if (lastSelectedId === null) {
      // No anchor, just select this one
      set({ selectedTorrentIds: new Set([id]), lastSelectedId: id });
      return;
    }

    if (selectedTorrentIds.has(id)) {
      let next = new Set(selectedTorrentIds);
      next.delete(id);
      set({ selectedTorrentIds: next });
      return;
    }

    const anchorIdx = orderedIds.indexOf(lastSelectedId);
    const targetIdx = orderedIds.indexOf(id);

    if (anchorIdx === -1 || targetIdx === -1) {
      // Fallback: just select the target
      set({ selectedTorrentIds: new Set([id]), lastSelectedId: id });
      return;
    }

    const startIdx = Math.min(anchorIdx, targetIdx);
    const endIdx = Math.max(anchorIdx, targetIdx);
    const rangeIds = orderedIds.slice(startIdx, endIdx + 1);

    // Extend selection with range
    const next = new Set(selectedTorrentIds);
    for (const rangeId of rangeIds) {
      next.add(rangeId);
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
    set({ selectedTorrentIds: new Set(), lastSelectedId: null });
  },

  selectAll: (ids) => {
    set({ selectedTorrentIds: new Set(ids) });
  },

  selectRelative: (direction, orderedIds) => {
    const { selectedTorrentIds, lastSelectedId } = get();
    if (orderedIds.length === 0) return;

    let currentIdx: number;

    if (selectedTorrentIds.size === 0) {
      // Nothing selected, select first or last based on direction
      const newId =
        direction === "down"
          ? orderedIds[0]
          : orderedIds[orderedIds.length - 1];
      set({ selectedTorrentIds: new Set([newId]), lastSelectedId: newId });
      return;
    }

    if (selectedTorrentIds.size === 1) {
      // Single selection, move from that
      const currentId = Array.from(selectedTorrentIds)[0];
      currentIdx = orderedIds.indexOf(currentId);
    } else {
      // Multiple selected, use lastSelectedId if valid, otherwise first selected
      if (lastSelectedId !== null && orderedIds.includes(lastSelectedId)) {
        currentIdx = orderedIds.indexOf(lastSelectedId);
      } else {
        // Find first selected in order
        currentIdx = orderedIds.findIndex((id) => selectedTorrentIds.has(id));
      }
    }

    if (currentIdx === -1) {
      // Fallback: select first
      const newId = orderedIds[0];
      set({ selectedTorrentIds: new Set([newId]), lastSelectedId: newId });
      return;
    }

    const newIdx =
      direction === "down"
        ? Math.min(currentIdx + 1, orderedIds.length - 1)
        : Math.max(currentIdx - 1, 0);

    const newId = orderedIds[newIdx];
    set({ selectedTorrentIds: new Set([newId]), lastSelectedId: newId });
  },

  detailsModalTorrentId: null,
  openDetailsModal: (id) =>
    set({
      detailsModalTorrentId: id,
      selectedTorrentIds: new Set([id]),
      lastSelectedId: id,
    }),
  closeDetailsModal: () => set({ detailsModalTorrentId: null }),
}));
