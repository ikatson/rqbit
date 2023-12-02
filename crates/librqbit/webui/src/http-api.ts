import { AddTorrentResponse, ErrorDetails, ListTorrentsResponse, RqbitAPI, TorrentDetails, TorrentStats } from "./api-types";

// Define API URL and base path
const apiUrl = (window.origin === 'null' || window.origin === 'http://localhost:3031') ? 'http://localhost:3030' : '';

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

export const API: RqbitAPI = {
    listTorrents: (): Promise<ListTorrentsResponse> => makeRequest('GET', '/torrents'),
    getTorrentDetails: (index: number): Promise<TorrentDetails> => {
        return makeRequest('GET', `/torrents/${index}`);
    },
    getTorrentStats: (index: number): Promise<TorrentStats> => {
        return makeRequest('GET', `/torrents/${index}/stats/v1`);
    },

    uploadTorrent: (data: string | File, opts?: {
        listOnly?: boolean,
        selectedFiles?: Array<number>,
        unpopularTorrent?: boolean,
        initialPeers?: Array<string> | null,
    }): Promise<AddTorrentResponse> => {
        opts = opts || {};
        let url = '/torrents?&overwrite=true';
        if (opts.listOnly) {
            url += '&list_only=true';
        }
        if (opts.selectedFiles != null) {
            url += `&only_files=${opts.selectedFiles.join(',')}`;
        }
        if (opts.unpopularTorrent) {
            url += '&peer_connect_timeout=20&peer_read_write_timeout=60';
        }
        if (opts.initialPeers) {
            url += `&initial_peers=${opts.initialPeers.join(',')}`;
        }
        if (typeof data === 'string') {
            url += '&is_url=true';
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