import { create } from "zustand";
import { TorrentListItem } from "../api-types";

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
  // Always update - stats change frequently and we need UI to reflect changes
  setTorrents: (torrents) => set({ torrents }),
  refreshTorrents: () => {},
  setRefreshTorrents: (callback) => set({ refreshTorrents: callback }),
}));
