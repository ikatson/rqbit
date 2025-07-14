import { create } from "zustand";
import { TorrentIdWithStats } from "../api-types";

export interface TorrentStore {
  torrents: Array<TorrentIdWithStats> | null;
  setTorrents: (torrents: Array<TorrentIdWithStats>) => void;

  torrentsInitiallyLoading: boolean;
  torrentsLoading: boolean;
  setTorrentsLoading: (loading: boolean) => void;

  refreshTorrents: () => void;
  setRefreshTorrents: (callback: () => void) => void;

  selectedTorrentIds: number[];
  setSelectedTorrentId: (torrent: number) => void;
  toggleSelectedTorrentId: (torrent: number) => void;
}

export const useTorrentStore = create<TorrentStore>((set) => ({
  torrents: null,
  torrentsLoading: false,
  torrentsInitiallyLoading: false,
  selectedTorrentIds: [],
  setSelectedTorrentId: (torrent) => set({ selectedTorrentIds: [torrent] }),
  toggleSelectedTorrentId: (torrent) =>
    set((prev) => ({
      selectedTorrentIds: prev.selectedTorrentIds.includes(torrent)
        ? prev.selectedTorrentIds.filter((id) => id !== torrent)
        : [...prev.selectedTorrentIds, torrent],
    })),
  setTorrentsLoading: (loading: boolean) =>
    set((prev) => {
      if (prev.torrents == null) {
        return { torrentsInitiallyLoading: loading, torrentsLoading: loading };
      }
      return { torrentsInitiallyLoading: false, torrentsLoading: loading };
    }),
  setTorrents: (torrents) =>
    set((prev) => {
      return { torrents };
    }),
  refreshTorrents: () => {},
  setRefreshTorrents: (callback) => set({ refreshTorrents: callback }),
}));
