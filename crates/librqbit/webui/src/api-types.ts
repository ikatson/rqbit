// Interface for the Torrent API response
export interface TorrentId {
  id: number;
  info_hash: string;
}

export interface TorrentFile {
  name: string;
  components: string[];
  length: number;
  included: boolean;
  attributes: TorrentFileAttributes;
}

export interface TorrentFileAttributes {
  symlink: boolean;
  hidden: boolean;
  padding: boolean;
  executable: boolean;
}

// Interface for the Torrent Details API response (with files, from individual endpoint)
export interface TorrentDetails {
  name: string | null;
  info_hash: string;
  files: Array<TorrentFile>;
  total_pieces?: number;
  output_folder: string;
}

// Interface for torrent list item (from bulk /torrents?with_stats=true endpoint)
// This matches TorrentDetailsResponse from the backend, but files are not included in the list
export interface TorrentListItem {
  id: number;
  info_hash: string;
  name: string | null;
  output_folder: string;
  total_pieces: number;
  stats?: TorrentStats;
}

export interface AddTorrentResponse {
  id: number | null;
  details: TorrentDetails;
  output_folder: string;
  seen_peers?: Array<string>;
}

export interface ListTorrentsResponse {
  torrents: Array<TorrentListItem>;
}

export interface Speed {
  mbps: number;
  human_readable: string;
}

export interface AggregatePeerStats {
  queued: number;
  connecting: number;
  live: number;
  seen: number;
  dead: number;
  not_needed: number;
}

export type ConnectionKind = "tcp" | "utp" | "socks";

export interface PeerCounters {
  incoming_connections: number;
  fetched_bytes: number;
  uploaded_bytes: number;
  total_time_connecting_ms: number;
  connection_attempts: number;
  connections: number;
  errors: number;
  fetched_chunks: number;
  downloaded_and_checked_pieces: number;
  total_piece_download_ms: number;
  times_stolen_from_me: number;
  times_i_stole: number;
}

export interface PeerStats {
  counters: PeerCounters;
  state: string;
  conn_kind: ConnectionKind | null;
}

export interface PeerStatsSnapshot {
  peers: Record<string, PeerStats>;
}

export interface ConnectionStatSingle {
  attempts: number;
  successes: number;
  errors: number;
}
export interface ConnectionStatsPerFamily {
  v4: ConnectionStatSingle;
  v6: ConnectionStatSingle;
}
export interface ConnectionStats {
  tcp: ConnectionStatsPerFamily;
  utp: ConnectionStatsPerFamily;
  socks: ConnectionStatsPerFamily;
}

export interface SessionCounters {
  fetched_bytes: number;
  uploaded_bytes: number;
  blocked_incoming: number;
  blocked_outgoing: number;
}

export interface SessionStats {
  counters: SessionCounters;
  peers: AggregatePeerStats;
  connections: ConnectionStats;
  download_speed: Speed;
  upload_speed: Speed;
  uptime_seconds: number;
}

export interface LimitsConfig {
  upload_bps?: number | null;
  download_bps?: number | null;
}

// Interface for the Torrent Stats API response
export interface LiveTorrentStats {
  snapshot: {
    have_bytes: number;
    downloaded_and_checked_bytes: number;
    downloaded_and_checked_pieces: number;
    fetched_bytes: number;
    uploaded_bytes: number;
    initially_needed_bytes: number;
    remaining_bytes: number;
    total_bytes: number;
    total_piece_download_ms: number;
    peer_stats: AggregatePeerStats;
  };
  average_piece_download_time: {
    secs: number;
    nanos: number;
  };
  download_speed: Speed;
  upload_speed: Speed;
  all_time_download_speed: {
    mbps: number;
    human_readable: string;
  };
  time_remaining: {
    human_readable: string;
    duration?: {
      secs: number;
    };
  } | null;
}

export const STATE_INITIALIZING = "initializing";
export const STATE_PAUSED = "paused";
export const STATE_LIVE = "live";
export const STATE_ERROR = "error";

export interface TorrentStats {
  state: "initializing" | "paused" | "live" | "error";
  error: string | null;
  file_progress: number[];
  progress_bytes: number;
  finished: boolean;
  total_bytes: number;
  live: LiveTorrentStats | null;
}

export interface ErrorDetails {
  id?: number;
  method?: string;
  path?: string;
  status?: number;
  statusText?: string;
  text: string | React.ReactNode;
}

export type Duration = number;

export interface PeerConnectionOptions {
  connect_timeout?: Duration | null;
  read_write_timeout?: Duration | null;
  keep_alive_interval?: Duration | null;
}

export interface AddTorrentOptions {
  paused?: boolean;
  only_files_regex?: string | null;
  only_files?: number[] | null;
  overwrite?: boolean;
  list_only?: boolean;
  output_folder?: string | null;
  sub_folder?: string | null;
  peer_opts?: PeerConnectionOptions | null;
  force_tracker_interval?: Duration | null;
  initial_peers?: string[] | null; // Assuming SocketAddr is equivalent to a string in TypeScript
  preferred_id?: number | null;
}

export type Value = string | number | boolean;

export interface Span {
  name: string;
  [key: string]: Value;
}

/*
Example log line

const EXAMPLE_LOG_JSON: JSONLogLine = {
  timestamp: "2023-12-08T21:48:13.649165Z",
  level: "DEBUG",
  fields: { message: "successfully port forwarded 192.168.0.112:4225" },
  target: "librqbit_upnp",
  span: { port: 4225, name: "manage_port" },
  spans: [
    { port: 4225, name: "upnp_forward" },
    {
      location: "http://192.168.0.1:49152/IGDdevicedesc_brlan0.xml",
      name: "upnp_endpoint",
    },
    { device: "ARRIS TG3492LG", name: "device" },
    { device: "WANDevice:1", name: "device" },
    { device: "WANConnectionDevice:1", name: "device" },
    { url: "/upnp/control/WANIPConnection0", name: "service" },
    { port: 4225, name: "manage_port" },
  ],
};
*/
export interface JSONLogLine {
  level: string;
  timestamp: string;
  fields: {
    message: string;
    [key: string]: Value;
  };
  target: string;
  span: Span;
  spans: Span[];
}

export interface RqbitAPI {
  getPlaylistUrl: (index: number) => string | null;
  getStreamLogsUrl: () => string | null;
  listTorrents: (opts?: {
    withStats?: boolean;
  }) => Promise<ListTorrentsResponse>;
  getTorrentDetails: (index: number) => Promise<TorrentDetails>;
  getTorrentStats: (index: number) => Promise<TorrentStats>;
  getTorrentHaves: (index: number) => Promise<Uint8Array>;
  getPeerStats: (index: number) => Promise<PeerStatsSnapshot>;
  getTorrentStreamUrl: (
    index: number,
    file_id: number,
    filename?: string | null,
  ) => string | null;
  uploadTorrent: (
    data: string | File,
    opts?: AddTorrentOptions,
  ) => Promise<AddTorrentResponse>;

  pause: (index: number) => Promise<void>;
  updateOnlyFiles: (index: number, files: number[]) => Promise<void>;
  start: (index: number) => Promise<void>;
  forget: (index: number) => Promise<void>;
  delete: (index: number) => Promise<void>;
  stats: () => Promise<SessionStats>;
  getLimits: () => Promise<LimitsConfig>;
  setLimits: (limits: LimitsConfig) => Promise<void>;
}
