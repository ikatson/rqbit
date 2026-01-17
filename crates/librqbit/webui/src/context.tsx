import { createContext } from "react";
import { LimitsConfig, RqbitAPI, SessionStats } from "./api-types";

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
  getPeerStats: () => {
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
  getStreamLogsUrl: function (): string | null {
    throw new Error("Function not implemented.");
  },
  getPlaylistUrl: function (index: number): string | null {
    throw new Error("Function not implemented.");
  },
  stats: function (): Promise<SessionStats> {
    throw new Error("Function not implemented.");
  },
  getTorrentHaves: function (index: number): Promise<Uint8Array> {
    throw new Error("Function not implemented.");
  },
  getLimits: function (): Promise<LimitsConfig> {
    throw new Error("Function not implemented.");
  },
  setLimits: function (limits: LimitsConfig): Promise<void> {
    throw new Error("Function not implemented.");
  },
});
