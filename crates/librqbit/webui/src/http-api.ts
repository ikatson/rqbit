import {
  AddTorrentOptions,
  AddTorrentResponse,
  ErrorDetails,
  ListTorrentsResponse,
  RqbitAPI,
  SessionStats,
  TorrentDetails,
  TorrentStats,
} from "./api-types";

// Define API URL and base path
const apiUrl = (() => {
  if (window.origin === "null" || window.origin === "http://localhost:3031") {
    return "http://localhost:3030"
  }
  let port = /http.*:\/\/.*:(\d+)/.exec(window.origin)?.[1];
  if (port == "3031") {
    return window.origin.replace("3031", "3030");
  }
  return "";
})();

// Wrapper around `fetch` to support a custom timeout.
// If specified, uses `AbortController` to timeout the pending fetch
function fetchWithTimeout(url: string, options: RequestInit = {}, timeout: number | undefined) {
  let pending;
  let timeoutId = undefined;
  if (timeout !== undefined) {
    const controller = new AbortController();
    const { signal } = controller;

    timeoutId = setTimeout(() => controller.abort(), timeout);
    console.log("Fetching with timeout: ", timeout);
    pending = fetch(url, { ...options, signal })
  } else {
    pending = fetch(url, options)
  }

  return pending
    .then(response => {
      clearTimeout(timeoutId);
      return response;
    })
    .catch(error => {
      clearTimeout(timeoutId);
      throw error;
    });
}

const makeRequest = async (
  method: string,
  path: string,
  data?: any,
  isJson?: boolean,
  timeout?: number,
): Promise<any> => {
  console.log(method, path);
  const url = apiUrl + path;
  let options: RequestInit = {
    method,
    headers: {
      Accept: "application/json",
    },
  };
  if (isJson) {
    options.headers = {
      Accept: "application/json",
      "Content-Type": "application/json",
    };
    options.body = JSON.stringify(data);
  } else {
    options.body = data;
  }

  let error: ErrorDetails = {
    method: method,
    path: path,
    text: "",
    timedOut: false,
  };

  let response: Response;

  try {
    response = await fetchWithTimeout(url, options, timeout);
  } catch (e: any) {
    console.log(e);
    if (e.name === "AbortError") {
      error.text = "fetch timed out after " + timeout + "ms"
      error.timedOut = true;
      return Promise.reject(error);
    }
    // else, generic network error
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
  stats: (): Promise<SessionStats> => {
    return makeRequest("GET", "/stats");
  },
  uploadTorrent: (data: string | File, opts: AddTorrentOptions, timeout: number | undefined): Promise<AddTorrentResponse> => {
    console.log("Uploading torrent with ", data, opts, timeout);
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
    return makeRequest("POST", url, data, undefined, timeout);
  },

  updateOnlyFiles: (index: number, files: number[]): Promise<void> => {
    let url = `/torrents/${index}/update_only_files`;
    return makeRequest(
      "POST",
      url,
      {
        only_files: files,
      },
      true,
    );
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
  getTorrentStreamUrl: (
    index: number,
    file_id: number,
    filename?: string | null,
  ) => {
    let url = apiUrl + `/torrents/${index}/stream/${file_id}`;
    if (!!filename) {
      url += `/${filename}`;
    }
    return url;
  },
  getPlaylistUrl: (index: number) => {
    return (apiUrl || window.origin) + `/torrents/${index}/playlist`;
  },
};
