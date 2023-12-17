import { create } from "zustand";
import { TorrentId } from "../api-types";

export interface TorrentStore {
  torrents: Array<TorrentId> | null;
  setTorrents: (torrents: Array<TorrentId>) => void;

  torrentsLoading: boolean;
  setTorrentsLoading: (loading: boolean) => void;

  refreshTorrents: () => void;
  setRefreshTorrents: (callback: () => void) => void;
}

export const useTorrentStore = create<TorrentStore>((set) => ({
  torrents: null,
  torrentsLoading: false,
  setTorrentsLoading: (loading: boolean) => set({ torrentsLoading: loading }),
  setTorrents: (torrents) => set({ torrents }),
  refreshTorrents: () => {},
  setRefreshTorrents: (callback) => set({ refreshTorrents: callback }),
}));
