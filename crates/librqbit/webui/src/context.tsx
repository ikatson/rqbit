import { createContext } from "react";
import { RqbitAPI, SessionStats } from "./api-types";

export const APIContext = createContext<RqbitAPI>({
  listTorrents: () => {
    throw new Error("Function not implemented.");
  },
  getTorrentDetails: () => {
    throw new Error("Function not implemented.");
  },
  getTorrentStats: () => {
    throw new Error("Function not implemented.");
  },

  getTorrentPeerStats: () => {
    throw new Error("Function not implemented.");
  },
  uploadTorrent: () => {
    throw new Error("Function not implemented.");
  },
  updateOnlyFiles: () => {
    throw new Error("Function not implemented.");
  },
  pause: () => {
    throw new Error("Function not implemented.");
  },
  start: () => {
    throw new Error("Function not implemented.");
  },
  forget: () => {
    throw new Error("Function not implemented.");
  },
  delete: () => {
    throw new Error("Function not implemented.");
  },
  getTorrentStreamUrl: () => {
    throw new Error("Function not implemented.");
  },
  getStreamLogsUrl: function (): string {
    throw new Error("Function not implemented.");
  },
  getPlaylistUrl: function (index: number): string {
    throw new Error("Function not implemented.");
  },
  stats: function (): Promise<SessionStats> {
    throw new Error("Function not implemented.");
  },
});
