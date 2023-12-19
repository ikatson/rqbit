import { createContext } from "react";
import { RqbitAPI } from "./api-types";
import { ContextType } from "./rqbit-web";

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
  uploadTorrent: () => {
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
  getStreamLogsUrl: () => {
    return null;
  },
});
export const RefreshTorrentStatsContext = createContext({ refresh: () => {} });
