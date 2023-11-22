import { StrictMode, createContext, memo, useContext, useEffect, useRef, useState } from 'react';
import ReactDOM from 'react-dom/client';
import { ProgressBar, Button, Container, Row, Col, Alert, Modal, Form, Spinner, Table } from 'react-bootstrap';

import 'bootstrap/dist/css/bootstrap.min.css';
import './styles.scss';

// Define API URL and base path
const apiUrl = (window.origin === 'null' || window.origin === 'http://localhost:3031') ? 'http://localhost:3030' : '';

interface ErrorDetails {
    id?: number,
    method?: string,
    path?: string,
    status?: number,
    statusText?: string,
    text: string,
};

interface Error {
    text: string,
    details?: ErrorDetails,
}

interface ContextType {
    setCloseableError: (error: Error) => void,
    setOtherError: (error: Error) => void,
    makeRequest: (method: string, path: string, data: any) => Promise<any>,
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

interface TorrentFile {
    name: string;
    length: number;
    included: boolean;
}

// Interface for the Torrent Details API response
interface TorrentDetails {
    info_hash: string,
    files: Array<TorrentFile>;
}

interface AddTorrentResponse {
    id: number | null;
    details: TorrentDetails;
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

const TorrentRow: React.FC<{
    detailsResponse: TorrentDetails, statsResponse: TorrentStats
}> = ({ detailsResponse, statsResponse }) => {
    const totalBytes = statsResponse.snapshot.total_bytes;
    const downloadedBytes = statsResponse.snapshot.have_bytes;
    const finished = totalBytes == downloadedBytes;
    const downloadPercentage = (downloadedBytes / totalBytes) * 100;

    return (
        <tr className="torrent-row">
            <Column label="Name">{getLargestFileName(detailsResponse)}</Column>
            <Column label="Size">{`${formatBytes(totalBytes)}`}</Column>
            <Column label="Progress">
                <ProgressBar now={downloadPercentage} label={`${downloadPercentage.toFixed(2)}%`} animated={!finished} style={{ 'minWidth': '200px' }} />
            </Column>
            <Column label="Download Speed">{statsResponse.download_speed.human_readable}</Column>
            <Column label="ETA">{getCompletionETA(statsResponse)}</Column>
            <Column label="Peers">{`${statsResponse.snapshot.peer_stats.live} / ${statsResponse.snapshot.peer_stats.seen}`}</Column>
        </tr >
    );
}

const Column: React.FC<{
    label: string,
    children?: any
}> = ({ children }) => (
    <td className='me-3 p-3'>
        {children}
    </td>
);

const Torrent = ({ torrent }) => {
    const defaultDetails: TorrentDetails = {
        info_hash: '',
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

    // Update details once
    useEffect(() => {
        if (detailsResponse === defaultDetails) {
            return loopUntilSuccess(async () => {
                await ctx.requests.getTorrentDetails(torrent.id).then(updateDetailsResponse);
            }, 1000);
        }
    }, [detailsResponse]);

    // Update stats forever.
    const update = async () => {
        const errorInterval = 10000;
        const liveInterval = 500;
        const finishedInterval = 5000;

        return ctx.requests.getTorrentStats(torrent.id).then((stats) => {
            updateStatsResponse(stats);
            return torrentIsDone(stats) ? finishedInterval : liveInterval;
        }, (e) => {
            return errorInterval
        })
    };

    useEffect(() => {
        let clear = customSetInterval(update, 0);
        return clear;
    }, []);

    return <TorrentRow detailsResponse={detailsResponse} statsResponse={statsResponse} />
}

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
        <Table className='text-center table-striped table-hover table-borderless'>
            <thead>
                <tr>
                    <th>Name</th>
                    <th>Size</th>
                    <th>Progress</th>
                    <th>Download Speed</th>
                    <th>ETA</th>
                    <th>Peers</th>
                </tr>
            </thead>
            <tbody>
                {props.torrents.map((t: TorrentId) =>
                    <Torrent key={t.id} torrent={t} />
                )}
            </tbody>

        </Table>
    )
};

const Root = () => {
    const [closeableError, setCloseableError] = useState<Error>(null);
    const [otherError, setOtherError] = useState<Error>(null);

    const [torrents, setTorrents] = useState<Array<TorrentId>>(null);
    const [torrentsLoading, setTorrentsLoading] = useState(false);

    const makeRequest = async (method: string, path: string, data: any): Promise<any> => {
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

    const requests = {
        getTorrentDetails: (index: number): Promise<TorrentDetails> => {
            return makeRequest('GET', `/torrents/${index}`, null);
        },
        getTorrentStats: (index: number): Promise<TorrentStats> => {
            return makeRequest('GET', `/torrents/${index}/stats`, null);
        }
    };

    const refreshTorrents = async () => {
        setTorrentsLoading(true);
        let torrents: { torrents: Array<TorrentId> } = await makeRequest('GET', '/torrents', null).finally(() => setTorrentsLoading(false));
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
                setOtherError({ text: 'Error refreshing torrents', details: e });
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
        <Container className='text-center'>
            <h1 className="mt-3 mb-4">rqbit web 0.0.1-alpha</h1>
            <RootContent
                closeableError={closeableError}
                otherError={otherError}
                torrents={torrents}
                torrentsLoading={torrentsLoading} />
        </Container>
    </AppContext.Provider >
}

const ErrorDetails = (props: { details: ErrorDetails }) => {
    let { details } = props;
    if (!details) {
        return null;
    }
    return <>
        {
            details.status && (
                <strong>{details.status} {details.statusText}: </strong>
            )
        }
        {details.text}
    </>

}

const ErrorComponent = (props: { error: Error, remove?: () => void }) => {
    let { error, remove } = props;

    if (error == null) {
        return null;
    }

    return (<Alert variant='danger' onClose={remove} dismissible={!!remove}>
        <Alert.Heading>{error.text}</Alert.Heading>

        <ErrorDetails details={error.details} />
    </Alert>);
};

const UploadButton = ({ buttonText, onClick, data, resetData, variant }) => {
    const [loading, setLoading] = useState(false);
    const [fileList, setFileList] = useState(null);
    const ctx = useContext(AppContext);
    const showModal = data !== null;

    // Get the torrent file list if there's data.
    useEffect(() => {
        if (data === null) {
            return;
        }

        let t = setTimeout(async () => {
            setLoading(true);
            try {
                const response: AddTorrentResponse = await ctx.makeRequest('POST', `/torrents?list_only=true&overwrite=true`, data);
                console.log(response);
                setFileList(response.details.files);
            } catch (e) {
                ctx.setCloseableError({ text: 'Error listing torrent', details: e });
                clear();
            } finally {
                setLoading(false);
            }
        }, 0);
        return () => clearTimeout(t);
    }, [data]);

    const clear = () => {
        resetData();
        setFileList(null);
        setLoading(false);
    }

    return (
        <>
            <Button variant={variant} onClick={onClick} className='m-1'>
                {buttonText}
            </Button>

            <FileSelectionModal
                show={showModal}
                onHide={clear}
                fileList={fileList}
                data={data}
                fileListLoading={loading}
            />
        </>
    );
};

const MagnetInput = () => {
    let [magnet, setMagnet] = useState(null);

    const onClick = () => {
        const m = prompt('Enter magnet link or HTTP(s) URL');
        setMagnet(m === '' ? null : m);
    };

    return (
        <UploadButton variant='primary' buttonText="Add Torrent from Magnet Link" onClick={onClick} data={magnet} resetData={() => setMagnet(null)} />
    );
};

const FileInput = () => {
    const inputRef = useRef<HTMLInputElement>();
    const [file, setFile] = useState(null);

    const onFileChange = async () => {
        const file = inputRef.current.files[0];
        setFile(file);
    };

    const reset = () => {
        inputRef.current.value = '';
        setFile(null);
    }

    const onClick = () => {
        inputRef.current.click();
    }

    return (
        <>
            <input type="file" ref={inputRef} accept=".torrent" onChange={onFileChange} className='d-none' />
            <UploadButton variant='secondary' buttonText="Upload .torrent File" onClick={onClick} data={file} resetData={reset} />
        </>
    );
};

const FileSelectionModal = (props: { show: boolean, onHide, fileList: Array<TorrentFile> | null, fileListLoading: boolean, data }) => {
    let { show, onHide, fileList, fileListLoading, data } = props;

    const [selectedFiles, setSelectedFiles] = useState([]);
    const [uploading, setUploading] = useState(false);
    const [uploadError, setUploadError] = useState<Error>(null);

    useEffect(() => {
        setSelectedFiles((fileList || []).map((_, id) => id));
    }, [fileList]);

    fileList = fileList || [];

    let ctx = useContext(AppContext);

    const clear = () => {
        onHide();
        setSelectedFiles([]);
        setUploadError(null);
        setUploading(false);
    }

    const handleToggleFile = (fileIndex: number) => {
        if (selectedFiles.includes(fileIndex)) {
            setSelectedFiles(selectedFiles.filter((index) => index !== fileIndex));
        } else {
            setSelectedFiles([...selectedFiles, fileIndex]);
        }
    };

    const handleUpload = async () => {
        const getSelectedFilesQueryParam = () => {
            let allPresent = true;
            fileList.map((_, id) => {
                allPresent = allPresent && selectedFiles.includes(id);
            });
            return allPresent ? '' : '&only_files=' + selectedFiles.join(',');
        };

        let url = `/torrents?overwrite=true${getSelectedFilesQueryParam()}`;

        setUploading(true);
        ctx.makeRequest('POST', url, data).then(() => { onHide() }, (e) => {
            setUploadError({ text: 'Error starting torrent', details: e });
        }).finally(() => setUploading(false));
    };

    return (
        <Modal show={show} onHide={clear} size='lg'>
            <Modal.Header closeButton>
                <Modal.Title>Select Files</Modal.Title>
            </Modal.Header>
            <Modal.Body>
                {fileListLoading ? (
                    <Spinner />
                ) : (
                    <>
                        <Form>

                            {fileList.map((file, index) => (
                                <Form.Group key={index} controlId={`check-${index}`}>
                                    <Form.Check

                                        type="checkbox"
                                        label={`${file.name}  (${formatBytes(file.length)})`}
                                        checked={selectedFiles.includes(index)}
                                        onChange={() => handleToggleFile(index)}>
                                    </Form.Check>
                                </Form.Group>
                            ))}

                        </Form>
                        <ErrorComponent error={uploadError} />
                    </>

                )}
            </Modal.Body>
            <Modal.Footer>
                {uploading && <Spinner />}
                <Button variant="secondary" onClick={clear}>
                    Cancel
                </Button>
                <Button variant="primary" onClick={handleUpload} disabled={fileListLoading || uploading || selectedFiles.length == 0}>
                    OK
                </Button>
            </Modal.Footer>
        </Modal >
    );
};

const Buttons = () => {
    return (
        <div id="buttons-container" className="mt-3">
            <MagnetInput />
            <FileInput />
        </div>
    );
};

const RootContent = (props: { closeableError: ErrorDetails, otherError: ErrorDetails, torrents: Array<TorrentId>, torrentsLoading: boolean }) => {
    let ctx = useContext(AppContext);
    return <Container>
        <ErrorComponent error={props.closeableError} remove={() => ctx.setCloseableError(null)} />
        <ErrorComponent error={props.otherError} />
        <TorrentsList torrents={props.torrents} loading={props.torrentsLoading} />
        <Buttons />
    </Container>
};

function torrentIsDone(stats: TorrentStats): boolean {
    return stats.snapshot.have_bytes == stats.snapshot.total_bytes;
}

// Render function to display all torrents
async function displayTorrents() {
    // Get the torrents container
    const torrentsContainer = document.getElementById('app');
    ReactDOM.createRoot(torrentsContainer).render(<StrictMode><Root /></StrictMode>);
}

// Function to format bytes to GB
function formatBytes(bytes) {
    if (bytes === 0) return '0 Bytes';

    const k = 1024;
    const sizes = ['Bytes', 'KB', 'MB', 'GB', 'TB', 'PB', 'EB', 'ZB', 'YB'];

    const i = Math.floor(Math.log(bytes) / Math.log(k));

    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
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

function loopUntilSuccess(callback, interval: number) {
    let timeoutId: number;

    const executeCallback = async () => {
        let retry = await callback().then(() => { false }, () => { true });
        if (retry) {
            scheduleNext();
        }
    }

    let scheduleNext = (i?: number) => {
        timeoutId = setTimeout(executeCallback, i !== undefined ? i : interval);
    }

    scheduleNext(0);

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