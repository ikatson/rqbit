import { create } from "zustand";
import { TorrentListItem } from "../api-types";

// Deep compare two torrents using JSON serialization
function torrentsEqual(a: TorrentListItem, b: TorrentListItem): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

export interface TorrentStore {
  torrents: Array<TorrentListItem> | null;
  setTorrents: (torrents: Array<TorrentListItem>) => void;

  torrentsInitiallyLoading: boolean;
  torrentsLoading: boolean;
  setTorrentsLoading: (loading: boolean) => void;

  refreshTorrents: () => void;
  setRefreshTorrents: (callback: () => void) => void;
}

export const useTorrentStore = create<TorrentStore>((set) => ({
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
}));
