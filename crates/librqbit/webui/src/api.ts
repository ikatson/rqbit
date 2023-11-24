// Define API URL and base path
const apiUrl = (window.origin === 'null' || window.origin === 'http://localhost:3031') ? 'http://localhost:3030' : '';

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

export interface TorrentStats {
    state: string,
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


const makeRequest = async (method: string, path: string, data?: any): Promise<any> => {
    console.log(method, path);
    const url = apiUrl + path;
    const options: RequestInit = {
        method,
        headers: {
            'Accept': 'application/json',
        },
        body: data,
    };

    let error: ErrorDetails = {
        method: method,
        path: path,
        text: ''
    };

    let response: Response;

    try {
        response = await fetch(url, options);
    } catch (e) {
        error.text = 'network error';
        return Promise.reject(error);
    }

    error.status = response.status;
    error.statusText = response.statusText;

    if (!response.ok) {
        const errorBody = await response.text();
        try {
            const json = JSON.parse(errorBody);
            error.text = json.human_readable !== undefined ? json.human_readable : JSON.stringify(json, null, 2);
        } catch (e) {
            error.text = errorBody;
        }
        return Promise.reject(error);
    }
    const result = await response.json();
    return result;
}

export const API = {
    listTorrents: (): Promise<ListTorrentsResponse> => makeRequest('GET', '/torrents'),
    getTorrentDetails: (index: number): Promise<TorrentDetails> => {
        return makeRequest('GET', `/torrents/${index}`);
    },
    getTorrentStats: (index: number): Promise<TorrentStats> => {
        return makeRequest('GET', `/torrents/${index}/stats/v1`);
    },

    uploadTorrent: (data: string | File, opts?: {
        listOnly?: boolean, selectedFiles?: Array<number>
    }): Promise<AddTorrentResponse> => {
        opts = opts || {};
        let url = '/torrents?&overwrite=true';
        if (opts.listOnly) {
            url += '&list_only=true';
        }
        if (opts.selectedFiles != null) {
            url += `&only_files=${opts.selectedFiles.join(',')}`;
        }
        return makeRequest('POST', url, data)
    },

    pause: (index: number): Promise<void> => {
        return makeRequest('POST', `/torrents/${index}/pause`);
    },

    start: (index: number): Promise<void> => {
        return makeRequest('POST', `/torrents/${index}/start`);
    },

    forget: (index: number): Promise<void> => {
        return makeRequest('POST', `/torrents/${index}/forget`);
    },

    delete: (index: number): Promise<void> => {
        return makeRequest('POST', `/torrents/${index}/delete`);
    }
}