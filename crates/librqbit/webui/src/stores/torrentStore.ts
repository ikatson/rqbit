import { create } from "zustand";
import { TorrentDetails, TorrentListItem } from "../api-types";

function deepEqual(obj1: any, obj2: any): boolean {
  // 1. Same reference or same primitive value
  if (obj1 === obj2) return true;

  // 2. Handle nulls and different types
  if (
    obj1 === null ||
    obj2 === null ||
    typeof obj1 !== "object" ||
    typeof obj2 !== "object"
  ) {
    return false;
  }

  // 3. Handle Arrays specifically
  if (Array.isArray(obj1) !== Array.isArray(obj2)) return false;

  const keys1 = Object.keys(obj1);
  const keys2 = Object.keys(obj2);

  // 4. Optimization: different number of properties means they aren't equal
  if (keys1.length !== keys2.length) return false;

  // 5. Recursive check for every key
  for (const key of keys1) {
    if (!keys2.includes(key) || !deepEqual(obj1[key], obj2[key])) {
      return false;
    }
  }

  return true;
}

// Deep compare two torrents
function torrentsEqual(a: TorrentListItem, b: TorrentListItem): boolean {
  return deepEqual(a, b);
}

export interface TorrentStore {
  torrents: Array<TorrentListItem> | null;
  setTorrents: (torrents: Array<TorrentListItem>) => void;

  torrentsInitiallyLoading: boolean;
  torrentsLoading: boolean;
  setTorrentsLoading: (loading: boolean) => void;

  refreshTorrents: () => void;
  setRefreshTorrents: (callback: () => void) => void;

  // TorrentDetails cache (keyed by torrent id)
  detailsCache: Map<number, TorrentDetails>;
  getDetails: (id: number) => TorrentDetails | null;
  setDetails: (id: number, details: TorrentDetails) => void;
}

export const useTorrentStore = create<TorrentStore>((set, get) => ({
  torrents: null,
  torrentsLoading: false,
  torrentsInitiallyLoading: false,
  setTorrentsLoading: (loading: boolean) =>
    set((prev) => {
      if (prev.torrents == null) {
        return { torrentsInitiallyLoading: loading, torrentsLoading: loading };
      }
      return { torrentsInitiallyLoading: false, torrentsLoading: loading };
    }),
  setTorrents: (newTorrents) =>
    set((prev) => {
      if (!prev.torrents) {
        return { torrents: newTorrents };
      }

      // Build map of current torrents for O(1) lookup
      const currentMap = new Map(prev.torrents.map((t) => [t.id, t]));

      // Reuse old reference if torrent unchanged
      const mergedTorrents = newTorrents.map((newTorrent) => {
        const current = currentMap.get(newTorrent.id);
        if (current && torrentsEqual(current, newTorrent)) {
          return current; // Keep old reference
        }
        return newTorrent;
      });

      // Check if array itself changed
      const arrayChanged =
        mergedTorrents.length !== prev.torrents.length ||
        mergedTorrents.some((t, i) => t !== prev.torrents![i]);

      return arrayChanged ? { torrents: mergedTorrents } : {};
    }),
  refreshTorrents: () => {},
  setRefreshTorrents: (callback) => set({ refreshTorrents: callback }),

  // TorrentDetails cache
  detailsCache: new Map(),
  getDetails: (id) => get().detailsCache.get(id) ?? null,
  setDetails: (id, details) =>
    set((prev) => ({
      detailsCache: new Map(prev.detailsCache).set(id, details),
    })),
}));
