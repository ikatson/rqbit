import { StrictMode, createContext, memo, useContext, useEffect, useRef, useState } from 'react';
import ReactDOM from 'react-dom/client';

// Define API URL and base path
const apiUrl = (window.origin === 'null' || window.origin === 'http://localhost:3031') ? 'http://localhost:3030' : '';

interface ErrorType {
    id?: number,
    method?: string,
    path?: string,
    status?: number,
    statusText?: string,
    text: string,
};

interface ContextType {
    setCloseableError: (error: ErrorType) => void,
    setOtherError: (error: ErrorType) => void,
    makeRequest: (method: string, path: string, data: any, showError: boolean) => Promise<any>,
    requests: {
        getTorrentDetails: any,
        getTorrentStats: any,
    },
    refreshTorrents: () => void,
}

const AppContext = createContext<ContextType>(null);

// Interface for the Torrent API response
interface TorrentId {
    id: number;
    info_hash: string;
}

// Interface for the Torrent Details API response
interface TorrentDetails {
    files: {
        name: string;
        length: number;
        included: boolean;
    }[];
}

// Interface for the Torrent Stats API response
interface TorrentStats {
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
    } | null;
}

function TorrentRow({ detailsResponse, statsResponse }) {
    const totalBytes = statsResponse.snapshot.total_bytes;
    const downloadedBytes = statsResponse.snapshot.have_bytes;
    const downloadPercentage = (downloadedBytes / totalBytes) * 100;

    return (
        <div className="torrent-row d-flex flex-row p-3 bg-light rounded mb-3">
            <Column label="Name" value={getLargestFileName(detailsResponse)} />
            <Column label="Size" value={`${formatBytesToGB(totalBytes)} GB`} />
            <ColumnWithProgressBar label="Progress" percentage={downloadPercentage} />
            <Column label="Download Speed" value={statsResponse.download_speed.human_readable} />
            <Column label="ETA" value={getCompletionETA(statsResponse)} />
            <Column label="Peers" value={`${statsResponse.snapshot.peer_stats.live} / ${statsResponse.snapshot.peer_stats.seen}`} />
        </div>
    );
}

const Column = ({ label, value }) => (
    <div className={`column-${label.toLowerCase().replace(" ", "-")} me-3 p-2`}>
        <p className="font-weight-bold">{label}</p>
        <p>{value}</p>
    </div>
);

const ColumnWithProgressBar = ({ label, percentage }) => (
    <div className="column-progress me-3 p-2">
        <p className="font-weight-bold">{label}</p>
        <div className="progress mb-1">
            <div className="progress-bar" style={{ width: `${percentage}%` }}></div>
        </div>
        <p className="mb-1">{percentage.toFixed(2)}%</p>
    </div >
);

const Torrent = ({ torrent }) => {
    const defaultDetails: TorrentDetails = {
        files: []
    };
    const defaultStats: TorrentStats = {
        snapshot: {
            have_bytes: 0,
            downloaded_and_checked_bytes: 0,
            downloaded_and_checked_pieces: 0,
            fetched_bytes: 0,
            uploaded_bytes: 0,
            initially_needed_bytes: 0,
            remaining_bytes: 0,
            total_bytes: 0,
            total_piece_download_ms: 0,
            peer_stats: {
                queued: 0,
                connecting: 0,
                live: 0,
                seen: 0,
                dead: 0,
                not_needed: 0
            }
        },
        average_piece_download_time: {
            secs: 0,
            nanos: 0
        },
        download_speed: {
            mbps: 0,
            human_readable: ''
        },
        all_time_download_speed: {
            mbps: 0,
            human_readable: ''
        },
        time_remaining: {
            human_readable: ''
        }
    };

    const [detailsResponse, updateDetailsResponse] = useState(defaultDetails);
    const [statsResponse, updateStatsResponse] = useState(defaultStats);

    let ctx = useContext(AppContext);

    const update = async () => {
        return await Promise.all([
            ctx.requests.getTorrentDetails(torrent.id).then((details) => {
                updateDetailsResponse(details);
                return details;
            }),
            ctx.requests.getTorrentStats(torrent.id).then((stats) => {
                updateStatsResponse(stats);
                return stats;
            })
        ]).then(([_, stats]) => {
            return torrentIsDone(stats) ? 10000 : 500;
        }, (e) => {
            return 5000;
        })
    };

    useEffect(() => {
        let clear = customSetInterval(update, 0);
        return clear;
    }, []);

    return <TorrentRow detailsResponse={detailsResponse} statsResponse={statsResponse} />
}

const Spinner = () => (
    <div className="spinner-border" role="status">
        <span className="sr-only">Loading...</span>
    </div>
)

const TorrentsList = (props: { torrents: Array<TorrentId>, loading: boolean }) => {
    if (props.torrents === null && props.loading) {
        return <Spinner />
    }
    // The app either just started, or there was an error loading torrents.
    if (props.torrents === null) {
        return <></>
    }

    if (props.torrents.length === 0) {
        return (
            <div className="text-center">
                <p>No existing torrents found. Add them through buttons below.</p>
            </div>
        )
    }
    return (
        <div>
            {props.torrents.map((t: TorrentId) =>
                <Torrent key={t.id} torrent={t} />
            )}
        </div>
    )
};

const Root = () => {
    const [closeableError, setCloseableError] = useState<ErrorType>(null);
    const [otherError, setOtherError] = useState<ErrorType>(null);

    const [torrents, setTorrents] = useState<Array<TorrentId>>(null);
    const [torrentsLoading, setTorrentsLoading] = useState(false);

    const makeRequest = async (method: string, path: string, data: any, showError: boolean): Promise<any> => {
        console.log(method, path);
        const url = apiUrl + path;
        const options: RequestInit = {
            method,
            headers: {
                'Accept': 'application/json',
            },
            body: data,
        };

        const maybeShowError = (e: ErrorType) => {
            if (showError) {
                setCloseableError(e);
            }
        }

        let error: ErrorType = {
            method: method,
            path: path,
            text: ''
        };

        let response: Response;

        try {
            response = await fetch(url, options);
        } catch (e) {
            error.text = 'unknown error: ' + e.toString();
            maybeShowError(error);
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
            maybeShowError(error);
            return Promise.reject(error);
        }
        const result = await response.json();
        return result;
    }

    const requests = {
        getTorrentDetails: (index: number): Promise<TorrentDetails> => {
            return makeRequest('GET', `/torrents/${index}`, null, false);
        },
        getTorrentStats: (index: number): Promise<TorrentStats> => {
            return makeRequest('GET', `/torrents/${index}/stats`, null, false);
        }
    };

    const refreshTorrents = async () => {
        setTorrentsLoading(true);
        let torrents: { torrents: Array<TorrentId> } = await makeRequest('GET', '/torrents', null, false).finally(() => setTorrentsLoading(false));
        setTorrents(torrents.torrents);
        return torrents;
    };

    useEffect(() => {
        let interval = 500;
        let clear = customSetInterval(async () => {
            try {
                await refreshTorrents();
                setOtherError(null);
                return interval;
            } catch (e) {
                setOtherError(e);
                console.error(e);
                return 5000;
            }
        }, interval);
        return clear;
    }, []);

    const context: ContextType = {
        setCloseableError,
        setOtherError,
        makeRequest,
        requests,
        refreshTorrents,
    }

    return <AppContext.Provider value={context}>
        <RootContent closeableError={closeableError} otherError={otherError} torrents={torrents} torrentsLoading={torrentsLoading} />
    </AppContext.Provider >
}

const Error = (props: { error: ErrorType, remove?: () => void }) => {
    let { error, remove } = props;

    if (error == null) {
        return null;
    }

    return (<div className="alert alert-danger fade show" role="alert">
        {error.method && (
            <strong>Error calling {error.method} {error.path}: </strong>
        )}
        {error.status && (
            <strong>{error.status} {error.statusText}: </strong>
        )}
        {error.text}
        {
            remove && (
                <button type="button" className="close" data-dismiss="alert" aria-label="Close" onClick={remove}>
                    <span aria-hidden="true">&times;</span>
                </button>
            )
        }
    </div >);
};

const MagnetInput = () => {
    let ctx = useContext(AppContext);

    // Function to add a torrent from a magnet link
    async function addTorrentFromMagnet(): Promise<void> {
        const magnetLink = prompt('Enter magnet link:');
        if (magnetLink) {
            await ctx.makeRequest('POST', '/torrents?overwrite=true', magnetLink, true);
            ctx.refreshTorrents();
        }
    }

    return <button id="add-magnet-button" className="btn btn-primary mr-2" onClick={addTorrentFromMagnet}>Add Torrent from Magnet Link</button>
};

const FileInput = () => {
    const inputRef = useRef<HTMLInputElement>();

    let ctx = useContext(AppContext);

    const inputOnChange = async (e) => {
        let file = e.target.files[0];
        await ctx.makeRequest('POST', '/torrents?overwrite=true', file, true);
        ctx.refreshTorrents();
    }

    const onClick = () => {
        inputRef.current.click();
    }

    return (<>
        <input type="file" ref={inputRef} id="file-input" accept=".torrent" onChange={inputOnChange} className='d-none' />
        <button id="upload-file-button" className="btn btn-secondary" onClick={onClick}>Upload .torrent File</button>
    </>);
};

const Buttons = () => {
    return (
        <div id="buttons-container" className="mt-3">
            <MagnetInput />
            <FileInput />
        </div>
    );
};

const LastErrors = (props: { lastErrors: Array<ErrorType> }) => {
    return <div>
        {props.lastErrors.map((e: ErrorType) => (
            <div className="alert alert-primary" role="alert"></div>
        ))}
    </div>
}

const RootContent = (props: { closeableError: ErrorType, otherError: ErrorType, torrents: Array<TorrentId>, torrentsLoading: boolean }) => {
    let ctx = useContext(AppContext);
    return <>
        <Error error={props.closeableError} remove={() => ctx.setCloseableError(null)} />
        <Error error={props.otherError} />
        <TorrentsList torrents={props.torrents} loading={props.torrentsLoading} />
        <Buttons />
    </>
};

function torrentIsDone(stats: TorrentStats): boolean {
    return stats.snapshot.have_bytes == stats.snapshot.total_bytes;
}

// Render function to display all torrents
async function displayTorrents() {
    // Get the torrents container
    const torrentsContainer = document.getElementById('output');
    const RootMemo = memo(Root, (prev, next) => true);
    ReactDOM.createRoot(torrentsContainer).render(<StrictMode><RootMemo /></StrictMode>);
}

// Function to format bytes to GB
function formatBytesToGB(bytes: number): string {
    const GB = bytes / (1024 * 1024 * 1024);
    return GB.toFixed(2);
}

// Function to get the name of the largest file in a torrent
function getLargestFileName(torrentDetails: TorrentDetails): string {
    if (torrentDetails.files.length == 0) {
        return 'Loading...';
    }
    const largestFile = torrentDetails.files.reduce((prev: any, current: any) => (prev.length > current.length) ? prev : current);
    return largestFile.name;
}

// Function to get the completion ETA of a torrent
function getCompletionETA(stats: TorrentStats): string {
    if (stats.time_remaining) {
        return stats.time_remaining.human_readable;
    } else {
        return 'N/A';
    }
}

function customSetInterval(asyncCallback: any, interval: number) {
    let timeoutId: number;
    let currentInterval: number = interval;

    const executeCallback = async () => {
        currentInterval = await asyncCallback();
        if (currentInterval === null || currentInterval === undefined) {
            throw 'asyncCallback returned null or undefined';
        }
        scheduleNext();
    }

    let scheduleNext = () => {
        timeoutId = setTimeout(executeCallback, currentInterval);
    }

    scheduleNext();

    let clearCustomInterval = () => {
        clearTimeout(timeoutId);
    }

    return clearCustomInterval;
}

// List all torrents on page load and set up auto-refresh
async function init(): Promise<void> {
    await displayTorrents();
}

// Call init function on page load
document.addEventListener('DOMContentLoaded', init);