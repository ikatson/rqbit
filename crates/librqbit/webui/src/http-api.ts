import {
  AddTorrentResponse,
  ErrorDetails,
  ListTorrentsResponse,
  RqbitAPI,
  TorrentDetails,
  TorrentStats,
} from "./api-types";

// Define API URL and base path
const apiUrl =
  window.origin === "null" || window.origin === "http://localhost:3031"
    ? "http://localhost:3030"
    : "";

const makeRequest = async (
  method: string,
  path: string,
  data?: any
): Promise<any> => {
  console.log(method, path);
  const url = apiUrl + path;
  const options: RequestInit = {
    method,
    headers: {
      Accept: "application/json",
    },
    body: data,
  };

  let error: ErrorDetails = {
    method: method,
    path: path,
    text: "",
  };

  let response: Response;

  try {
    response = await fetch(url, options);
  } catch (e) {
    error.text = "network error";
    return Promise.reject(error);
  }

  error.status = response.status;
  error.statusText = `${response.status} ${response.statusText}`;

  if (!response.ok) {
    const errorBody = await response.text();
    try {
      const json = JSON.parse(errorBody);
      error.text =
        json.human_readable !== undefined
          ? json.human_readable
          : JSON.stringify(json, null, 2);
    } catch (e) {
      error.text = errorBody;
    }
    return Promise.reject(error);
  }
  const result = await response.json();
  return result;
};

export const API: RqbitAPI & { getVersion: () => Promise<string> } = {
  getStreamLogsUrl: () => apiUrl + "/stream_logs",
  listTorrents: (): Promise<ListTorrentsResponse> =>
    makeRequest("GET", "/torrents"),
  getTorrentDetails: (index: number): Promise<TorrentDetails> => {
    return makeRequest("GET", `/torrents/${index}`);
  },
  getTorrentStats: (index: number): Promise<TorrentStats> => {
    return makeRequest("GET", `/torrents/${index}/stats/v1`);
  },

  uploadTorrent: (data, opts): Promise<AddTorrentResponse> => {
    let url = "/torrents?&overwrite=true";
    if (opts?.list_only) {
      url += "&list_only=true";
    }
    if (opts?.only_files != null) {
      url += `&only_files=${opts.only_files.join(",")}`;
    }
    if (opts?.peer_opts?.connect_timeout) {
      url += `&peer_connect_timeout=${opts.peer_opts.connect_timeout}`;
    }
    if (opts?.peer_opts?.read_write_timeout) {
      url += `&peer_read_write_timeout=${opts.peer_opts.read_write_timeout}`;
    }
    if (opts?.initial_peers) {
      url += `&initial_peers=${opts.initial_peers.join(",")}`;
    }
    if (opts?.output_folder) {
      url += `&output_folder=${opts.output_folder}`;
    }
    if (typeof data === "string") {
      url += "&is_url=true";
    }
    return makeRequest("POST", url, data);
  },

  pause: (index: number): Promise<void> => {
    return makeRequest("POST", `/torrents/${index}/pause`);
  },

  start: (index: number): Promise<void> => {
    return makeRequest("POST", `/torrents/${index}/start`);
  },

  forget: (index: number): Promise<void> => {
    return makeRequest("POST", `/torrents/${index}/forget`);
  },

  delete: (index: number): Promise<void> => {
    return makeRequest("POST", `/torrents/${index}/delete`);
  },
  getVersion: async (): Promise<string> => {
    const r = await makeRequest("GET", "/");
    return r.version;
  },
};
