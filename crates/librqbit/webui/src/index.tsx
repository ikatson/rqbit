import { StrictMode, createContext, useContext, useEffect, useRef, useState } from 'react';
import ReactDOM from 'react-dom/client';
import { ProgressBar, Button, Container, Row, Col, Alert, Modal, Form, Spinner, Table } from 'react-bootstrap';
import { AddTorrentResponse, TorrentDetails, TorrentFile, TorrentId, TorrentStats, ErrorDetails, API } from './api';

interface Error {
    text: string,
    details?: ErrorDetails,
}

interface ContextType {
    setCloseableError: (error: Error) => void,
    refreshTorrents: () => void,
}

const AppContext = createContext<ContextType>(null);

const TorrentRow: React.FC<{
    id: number, detailsResponse: TorrentDetails, statsResponse: TorrentStats
}> = ({ id, detailsResponse, statsResponse }) => {
    const totalBytes = statsResponse?.snapshot?.total_bytes ?? 1;
    const downloadedBytes = statsResponse?.snapshot?.have_bytes ?? 0;
    const finished = totalBytes == downloadedBytes;
    const downloadPercentage = (downloadedBytes / totalBytes) * 100;

    return (
        <Row className={`${id % 2 == 0 ? 'bg-light' : ''}`}>
            <Column size={4} label="Name">
                {detailsResponse ?
                    <div className='text-truncate'>
                        {getLargestFileName(detailsResponse)}
                    </div>
                    : <Spinner />}
            </Column>
            {statsResponse ?
                <>
                    <Column label="Size">{`${formatBytes(totalBytes)} `}</Column>
                    <Column size={2} label="Progress">
                        <ProgressBar now={downloadPercentage} label={`${downloadPercentage.toFixed(2)}% `} animated={!finished} />
                    </Column>
                    <Column size={2} label="Down Speed">{statsResponse.download_speed.human_readable}</Column>
                    <Column label="ETA">{getCompletionETA(statsResponse)}</Column>
                    <Column size={2} label="Peers">{`${statsResponse.snapshot.peer_stats.live} / ${statsResponse.snapshot.peer_stats.seen}`}</Column >
                </>
                : <Column label="Loading stats" size={8}><Spinner /></Column>
            }

        </Row >
    );
}

const Column: React.FC<{
    label: string,
    size?: number,
    children?: any
}> = ({ size, label, children }) => (
    <Col md={size || 1} className='py-3'>
        <div className='fw-bold'>{label}</div>
        {children}
    </Col>
);

const Torrent = ({ id, torrent }) => {
    const [detailsResponse, updateDetailsResponse] = useState<TorrentDetails>(null);
    const [statsResponse, updateStatsResponse] = useState<TorrentStats>(null);

    // Update details once.
    useEffect(() => {
        if (detailsResponse === null) {
            return loopUntilSuccess(async () => {
                await API.getTorrentDetails(torrent.id).then(updateDetailsResponse);
            }, 1000);
        }
    }, [detailsResponse]);

    // Update stats once then forever.
    useEffect(() => customSetInterval((async () => {
        const errorInterval = 10000;
        const liveInterval = 500;
        const finishedInterval = 5000;

        return API.getTorrentStats(torrent.id).then((stats) => {
            updateStatsResponse(stats);
            return torrentIsDone(stats) ? finishedInterval : liveInterval;
        }, (e) => {
            return errorInterval;
        });
    }), 0), []);

    return <TorrentRow id={id} detailsResponse={detailsResponse} statsResponse={statsResponse} />
}

const TorrentsList = (props: { torrents: Array<TorrentId>, loading: boolean }) => {
    if (props.torrents === null && props.loading) {
        return <Spinner />
    }
    // The app either just started, or there was an error loading torrents.
    if (props.torrents === null) {
        return;
    }

    if (props.torrents.length === 0) {
        return <div className="text-center">
            <p>No existing torrents found. Add them through buttons below.</p>
        </div>;
    }
    return <>
        {props.torrents.map((t: TorrentId) =>
            <Torrent id={t.id} key={t.id} torrent={t} />
        )}
    </>;
};

const Root = () => {
    const [closeableError, setCloseableError] = useState<Error>(null);
    const [otherError, setOtherError] = useState<Error>(null);

    const [torrents, setTorrents] = useState<Array<TorrentId>>(null);
    const [torrentsLoading, setTorrentsLoading] = useState(false);

    const refreshTorrents = async () => {
        setTorrentsLoading(true);
        let torrents = await API.listTorrents().finally(() => setTorrentsLoading(false));
        setTorrents(torrents.torrents);
    };

    useEffect(() => {
        return customSetInterval(async () =>
            refreshTorrents().then(() => {
                setOtherError(null);
                return 5000;
            }, (e) => {
                setOtherError({ text: 'Error refreshing torrents', details: e });
                console.error(e);
                return 5000;
            }), 0);
    }, []);

    const context: ContextType = {
        setCloseableError,
        refreshTorrents,
    }

    return <AppContext.Provider value={context}>
        <div className='text-center'>
            <h1 className="mt-3 mb-4">rqbit web 0.0.1-alpha</h1>
            <RootContent
                closeableError={closeableError}
                otherError={otherError}
                torrents={torrents}
                torrentsLoading={torrentsLoading} />
        </div>
    </AppContext.Provider >
}

const ErrorDetails = (props: { details: ErrorDetails }) => {
    let { details } = props;
    if (!details) {
        return null;
    }
    return <>
        {details.status && <strong>{details.status} {details.statusText}: </strong>}
        {details.text}
    </>

}

const ErrorComponent = (props: { error: Error, remove?: () => void }) => {
    let { error, remove } = props;

    if (error == null) {
        return null;
    }

    return (<Alert variant='danger' onClose={remove} dismissible={remove != null}>
        <Alert.Heading>{error.text}</Alert.Heading>

        <ErrorDetails details={error.details} />
    </Alert>);
};

const UploadButton = ({ buttonText, onClick, data, resetData, variant }) => {
    const [loading, setLoading] = useState(false);
    const [fileList, setFileList] = useState([]);
    const [fileListError, setFileListError] = useState(null);
    const ctx = useContext(AppContext);

    const showModal = data !== null || fileListError !== null;

    // Get the torrent file list if there's data.
    useEffect(() => {
        if (data === null) {
            return;
        }

        let t = setTimeout(async () => {
            setLoading(true);
            try {
                const response = await API.uploadTorrent(data, { listOnly: true });
                setFileList(response.details.files);
            } catch (e) {
                setFileListError({ text: 'Error listing torrent', details: e });
            } finally {
                setLoading(false);
            }
        }, 0);
        return () => clearTimeout(t);
    }, [data]);

    const clear = () => {
        resetData();
        setFileListError(null);
        setFileList([]);
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
                fileListError={fileListError}
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

const FileSelectionModal = (props: {
    show: boolean,
    onHide: () => void,
    fileList: Array<TorrentFile>,
    fileListError: Error,
    fileListLoading: boolean,
    data: string | File
}) => {
    let { show, onHide, fileList, fileListError, fileListLoading, data } = props;

    const [selectedFiles, setSelectedFiles] = useState([]);
    const [uploading, setUploading] = useState(false);
    const [uploadError, setUploadError] = useState<Error>(null);
    const ctx = useContext(AppContext);

    useEffect(() => {
        setSelectedFiles(fileList.map((_, id) => id));
    }, [fileList]);

    const clear = () => {
        onHide();
        setSelectedFiles([]);
        setUploadError(null);
        setUploading(false);
    }

    const handleToggleFile = (toggledId: number) => {
        if (selectedFiles.includes(toggledId)) {
            setSelectedFiles(selectedFiles.filter((i) => i !== toggledId));
        } else {
            setSelectedFiles([...selectedFiles, toggledId]);
        }
    };

    const handleUpload = async () => {
        setUploading(true);
        API.uploadTorrent(data, { selectedFiles }).then(
            () => {
                onHide();
                ctx.refreshTorrents();
            },
            (e) => {
                setUploadError({ text: 'Error starting torrent', details: e });
            }
        ).finally(() => setUploading(false));
    };

    return (
        <Modal show={show} onHide={clear} size='lg'>
            <Modal.Header closeButton>
                {!!fileListError || <Modal.Title>Select Files</Modal.Title>}
            </Modal.Header>
            <Modal.Body>
                {fileListLoading ? <Spinner />
                    : fileListError ? <ErrorComponent error={fileListError}></ErrorComponent> :
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
                }
                <ErrorComponent error={uploadError} />
            </Modal.Body>
            <Modal.Footer>
                {uploading && <Spinner />}
                <Button variant="primary" onClick={handleUpload} disabled={fileListLoading || uploading || selectedFiles.length == 0}>
                    OK
                </Button>
                <Button variant="secondary" onClick={clear}>
                    Cancel
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

function formatBytes(bytes: number): string {
    if (bytes === 0) return '0 Bytes';

    const k = 1024;
    const sizes = ['Bytes', 'KB', 'MB', 'GB'];

    const i = Math.floor(Math.log(bytes) / Math.log(k));

    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
}

function getLargestFileName(torrentDetails: TorrentDetails): string {
    const largestFile = torrentDetails.files.filter(
        (f) => f.included
    ).reduce(
        (prev: any, current: any) => (prev.length > current.length) ? prev : current
    );
    return largestFile.name;
}

function getCompletionETA(stats: TorrentStats): string {
    if (stats.time_remaining && stats.time_remaining.duration) {
        return formatSecondsToTime(stats.time_remaining.duration.secs);
    } else {
        return 'N/A';
    }
}

function formatSecondsToTime(seconds: number): string {
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    const remainingSeconds = seconds % 60;

    const formatUnit = (value: number, unit: string) => (value > 0 ? `${value}${unit}` : '');

    if (hours > 0) {
        return `${formatUnit(hours, 'h')} ${formatUnit(minutes, 'm')}`.trim();
    } else if (minutes > 0) {
        return `${formatUnit(minutes, 'm')} ${formatUnit(remainingSeconds, 's')}`.trim();
    } else {
        return `${formatUnit(remainingSeconds, 's')}`.trim();
    }
}

// Run a function with initial interval, then run it forever with the interval that the
// callback returns.
// Returns a callback to clear it.
function customSetInterval(asyncCallback: () => Promise<number>, initialInterval: number): () => void {
    let timeoutId: number;
    let currentInterval: number = initialInterval;

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

    return () => {
        clearTimeout(timeoutId);
    };
}

function loopUntilSuccess<T>(callback: () => Promise<T>, interval: number): () => void {
    let timeoutId: number;

    const executeCallback = async () => {
        let retry = await callback().then(() => false, () => true);
        if (retry) {
            scheduleNext();
        }
    }

    let scheduleNext = (overrideInterval?: number) => {
        timeoutId = setTimeout(executeCallback, overrideInterval !== undefined ? overrideInterval : interval);
    }

    scheduleNext(0);

    return () => clearTimeout(timeoutId);
}

// List all torrents on page load and set up auto-refresh
async function init(): Promise<void> {
    const torrentsContainer = document.getElementById('app');
    ReactDOM.createRoot(torrentsContainer).render(<StrictMode><Root /></StrictMode>);
}

// Call init function on page load
document.addEventListener('DOMContentLoaded', init);