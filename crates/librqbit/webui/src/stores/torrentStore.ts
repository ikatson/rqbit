import { create } from "zustand";
import { TorrentId } from "../api-types";

export interface TorrentStore {
  torrents: Array<TorrentId> | null;
  setTorrents: (torrents: Array<TorrentId>) => void;

  torrentsInitiallyLoading: boolean;
  torrentsLoading: boolean;
  setTorrentsLoading: (loading: boolean) => void;

  refreshTorrents: () => void;
  setRefreshTorrents: (callback: () => void) => void;
}

const torrentIdEquals = (t1: TorrentId, t2: TorrentId): boolean => {
  return t1.id == t2.id && t1.info_hash == t2.info_hash;
};

const torrentsEquals = (t1: TorrentId[] | null, t2: TorrentId[] | null) => {
  if (t1 === null && t2 === null) {
    return true;
  }

  if (t1 === null || t2 === null) {
    return false;
  }

  return (
    t1.length === t2.length && t1.every((t, i) => torrentIdEquals(t, t2[i]))
  );
};

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
  setTorrents: (torrents) =>
    set((prev) => {
      if (torrentsEquals(prev.torrents, torrents)) {
        return {};
      }
      return { torrents };
    }),
  refreshTorrents: () => {},
  setRefreshTorrents: (callback) => set({ refreshTorrents: callback }),
}));
