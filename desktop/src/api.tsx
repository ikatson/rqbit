import { RqbitDesktopConfig } from "./configuration";
import {
  AddTorrentResponse,
  LimitsConfig,
  ListTorrentsResponse,
  RqbitAPI,
  TorrentDetails,
  TorrentStats,
  ErrorDetails,
  SessionStats,
  PeerStatsSnapshot,
} from "rqbit-webui/src/api-types";

import { InvokeArgs, invoke } from "@tauri-apps/api/core";

interface InvokeErrorResponse {
  error_kind: string;
  human_readable: string;
  status: number;
  status_text: string;
}

function errorToUIError(
  path: string,
): (e: InvokeErrorResponse) => Promise<never> {
  return (e: InvokeErrorResponse) => {
    console.log(e);
    let reason: ErrorDetails = {
      method: "INVOKE",
      path: path,
      text: e.human_readable ?? e.toString(),
      status: e.status,
      statusText: e.status_text,
    };
    return Promise.reject(reason);
  };
}

export async function invokeAPI<Response>(
  name: string,
  params?: InvokeArgs,
): Promise<Response> {
  console.log("invoking", name, params);
  const result = await invoke<Response>(name, params).catch(
    errorToUIError(name),
  );
  console.log(result);
  return result;
}

async function readFileAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();

    reader.onload = function (event) {
      const base64String = (event?.target?.result as string)?.split(",")[1];
      if (base64String) {
        resolve(base64String);
      } else {
        reject(new Error("Failed to read file as base64."));
      }
    };

    reader.onerror = function (error) {
      console.log(error);
      reject(error);
    };

    reader.readAsDataURL(file);
  });
}

export const makeAPI = (configuration: RqbitDesktopConfig): RqbitAPI => {
  const getHttpBase = () => {
    if (!configuration.http_api.listen_addr) {
      return null;
    }
    let port = configuration.http_api.listen_addr.split(":")[1];
    if (!port) {
      return null;
    }

    return `http://127.0.0.1:${port}`;
  };

  let httpBase = getHttpBase();

  return {
    getStreamLogsUrl: () => {
      if (!httpBase) {
        return null;
      }
      return `${httpBase}/stream_logs`;
    },
    listTorrents: async function (): Promise<ListTorrentsResponse> {
      return await invokeAPI<ListTorrentsResponse>("torrents_list");
    },
    getTorrentDetails: async function (id: number): Promise<TorrentDetails> {
      return await invokeAPI<TorrentDetails>("torrent_details", { id });
    },
    getTorrentStats: async function (id: number): Promise<TorrentStats> {
      return await invokeAPI<TorrentStats>("torrent_stats", { id });
    },
    getTorrentHaves: async function (id: number): Promise<Uint8Array> {
      return new Uint8Array(
        await invokeAPI<ArrayBuffer>("torrent_haves", { id }),
      );
    },
    getPeerStats: async function (id: number): Promise<PeerStatsSnapshot> {
      return await invokeAPI<PeerStatsSnapshot>("torrent_peer_stats", {
        id,
        filter: { state: "live" },
      });
    },
    uploadTorrent: async function (data, opts): Promise<AddTorrentResponse> {
      if (data instanceof File) {
        let contents = await readFileAsBase64(data);
        return await invokeAPI<AddTorrentResponse>(
          "torrent_create_from_base64_file",
          {
            contents,
            opts: opts ?? {},
          },
        );
      }
      return await invokeAPI<AddTorrentResponse>("torrent_create_from_url", {
        url: data,
        opts: opts ?? {},
      });
    },
    updateOnlyFiles: function (id, files): Promise<void> {
      return invokeAPI<void>("torrent_action_configure", {
        id: id,
        onlyFiles: files,
      });
    },
    pause: function (id: number): Promise<void> {
      return invokeAPI<void>("torrent_action_pause", { id });
    },
    start: function (id: number): Promise<void> {
      return invokeAPI<void>("torrent_action_start", { id });
    },
    forget: function (id: number): Promise<void> {
      return invokeAPI<void>("torrent_action_forget", { id });
    },
    delete: function (id: number): Promise<void> {
      return invokeAPI<void>("torrent_action_delete", { id });
    },
    getTorrentStreamUrl: () => {
      return "";
    },
    getPlaylistUrl: (index: number) => {
      if (!httpBase) {
        return null;
      }
      return `${httpBase}/torrents/${index}/playlist`;
    },
    stats: () => {
      return invokeAPI<SessionStats>("stats");
    },
    getLimits: () => {
      // Desktop manages rate limits via config, not separate API
      return Promise.resolve({ upload_bps: null, download_bps: null });
    },
    setLimits: (_limits: LimitsConfig) => {
      // Desktop manages rate limits via config change, not this API
      return Promise.resolve();
    },
  };
};
