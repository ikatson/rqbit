// Interface for the Torrent API response
export interface TorrentId {
    id: number;
    info_hash: string;
}

export interface TorrentFile {
    name: string;
    length: number;
    included: boolean;
}

// Interface for the Torrent Details API response
export interface TorrentDetails {
    info_hash: string,
    files: Array<TorrentFile>;
}

export interface AddTorrentResponse {
    id: number | null;
    details: TorrentDetails;
    seen_peers?: Array<string>;
}

export interface ListTorrentsResponse {
    torrents: Array<TorrentId>;
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
        peer_stats: {
            queued: number;
            connecting: number;
            live: number;
            seen: number;
            dead: number;
            not_needed: number;
        };
    };
    average_piece_download_time: {
        secs: number;
        nanos: number;
    };
    download_speed: {
        mbps: number;
        human_readable: string;
    };
    all_time_download_speed: {
        mbps: number;
        human_readable: string;
    };
    time_remaining: {
        human_readable: string;
        duration?: {
            secs: number,
        }
    } | null;
}

export const STATE_INITIALIZING = 'initializing';
export const STATE_PAUSED = 'paused';
export const STATE_LIVE = 'live';
export const STATE_ERROR = 'error';

export interface TorrentStats {
    state: 'initializing' | 'paused' | 'live' | 'error',
    error: string | null,
    progress_bytes: number,
    finished: boolean,
    total_bytes: number,
    live: LiveTorrentStats | null;
}


export interface ErrorDetails {
    id?: number,
    method?: string,
    path?: string,
    status?: number,
    statusText?: string,
    text: string,
};

export interface RqbitAPI {
    listTorrents: () => Promise<ListTorrentsResponse>,
    getTorrentDetails: (index: number) => Promise<TorrentDetails>,
    getTorrentStats: (index: number) => Promise<TorrentStats>;
    uploadTorrent: (data: string | File, opts?: {
        listOnly?: boolean,
        selectedFiles?: Array<number>,
        unpopularTorrent?: boolean,
        initialPeers?: Array<string>,
    }) => Promise<AddTorrentResponse>;

    pause: (index: number) => Promise<void>;
    start: (index: number) => Promise<void>;
    forget: (index: number) => Promise<void>;
    delete: (index: number) => Promise<void>;
}