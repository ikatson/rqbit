import { MouseEventHandler, StrictMode, createContext, useContext, useEffect, useRef, useState } from 'react';
import ReactDOM from 'react-dom/client';
import { ProgressBar, Button, Container, Row, Col, Alert, Modal, Form, Spinner, Table } from 'react-bootstrap';
import { AddTorrentResponse, TorrentDetails, TorrentFile, TorrentId, TorrentStats, ErrorDetails, API, STATE_INITIALIZING, STATE_LIVE, STATE_PAUSED, STATE_ERROR } from './api';

interface Error {
    text: string,
    details?: ErrorDetails,
}

interface ContextType {
    setCloseableError: (error: Error) => void,
    refreshTorrents: () => void,
}

const AppContext = createContext<ContextType>(null);
const RefreshTorrentStatsContext = createContext<{ refresh: () => void }>(null);

const IconButton: React.FC<{
    className: string,
    onClick: () => void,
    disabled?: boolean,
    color?: string,
}> = ({ className, onClick, disabled, color }) => {
    const onClickStopPropagation = (e) => {
        e.stopPropagation();
        if (disabled) {
            return;
        }
        onClick();
    }
    return <a className={`bi ${className} p-1`} onClick={onClickStopPropagation} href='#'></a>
}

const DeleteTorrentModal = ({ id, show, onHide }) => {
    if (!show) {
        return null;
    }
    const [deleteFiles, setDeleteFiles] = useState(false);
    const [error, setError] = useState<Error>(null);
    const [deleting, setDeleting] = useState(false);

    const ctx = useContext(AppContext);

    const close = () => {
        setDeleteFiles(false);
        setError(null);
        setDeleting(false);
        onHide();
    }

    const deleteTorrent = () => {
        setDeleting(true);

        const call = deleteFiles ? API.delete : API.forget;

        call(id).then(() => {
            ctx.refreshTorrents();
            close();
        }).catch((e) => {
            setError({
                text: `Error deleting torrent id=${id}`,
                details: e,
            });
            setDeleting(false);
        })
    }

    return <Modal show={show} onHide={close}>
        <Modal.Header closeButton>
            Delete torrent
        </Modal.Header>
        <Modal.Body>
            <Form>
                <Form.Group controlId='delete-torrent'>
                    <Form.Check
                        type="checkbox"
                        label='Also delete files'
                        checked={deleteFiles}
                        onChange={() => setDeleteFiles(!deleteFiles)}>
                    </Form.Check>
                </Form.Group>
            </Form>
            {error && <ErrorComponent error={error} />}
        </Modal.Body>
        <Modal.Footer>
            {deleting && <Spinner />}
            <Button variant="primary" onClick={deleteTorrent} disabled={deleting}>
                OK
            </Button>
            <Button variant="secondary" onClick={close}>
                Cancel
            </Button>
        </Modal.Footer>
    </Modal>
}

const TorrentActions: React.FC<{
    id: number, statsResponse: TorrentStats
}> = ({ id, statsResponse }) => {
    let state = statsResponse.state;

    let [disabled, setDisabled] = useState<boolean>(false);
    let [deleting, setDeleting] = useState<boolean>(false);

    let refreshCtx = useContext(RefreshTorrentStatsContext);

    const canPause = state == 'live';
    const canUnpause = state == 'paused' || state == 'error';

    const ctx = useContext(AppContext);

    const unpause = () => {
        setDisabled(true);
        API.start(id).then(() => { refreshCtx.refresh() }, (e) => {
            ctx.setCloseableError({
                text: `Error starting torrent id=${id}`,
                details: e,
            });
        }).finally(() => setDisabled(false))
    };

    const pause = () => {
        setDisabled(true);
        API.pause(id).then(() => { refreshCtx.refresh() }, (e) => {
            ctx.setCloseableError({
                text: `Error pausing torrent id=${id}`,
                details: e,
            });
        }).finally(() => setDisabled(false))
    };

    const startDeleting = () => {
        setDisabled(true);
        setDeleting(true);
    }

    const cancelDeleting = () => {
        setDisabled(false);
        setDeleting(false);
    }

    return <Row>
        <Col>
            {canUnpause && <IconButton className="bi-play-circle" onClick={unpause} disabled={disabled} color='success' />}
            {canPause && <IconButton className="bi-pause-circle" onClick={pause} disabled={disabled} />}
            <IconButton className="bi-x-circle" onClick={startDeleting} disabled={disabled} color='danger' />
            <DeleteTorrentModal id={id} show={deleting} onHide={cancelDeleting} />
        </Col>
    </Row>
}

const TorrentRow: React.FC<{
    id: number, detailsResponse: TorrentDetails, statsResponse: TorrentStats
}> = ({ id, detailsResponse, statsResponse }) => {
    const state = statsResponse?.state ?? "";
    const error = statsResponse?.error;
    const totalBytes = statsResponse?.total_bytes ?? 1;
    const progressBytes = statsResponse?.progress_bytes ?? 0;
    const finished = statsResponse?.finished || false;
    const progressPercentage = error ? 100 : (progressBytes / totalBytes) * 100;
    const isAnimated = (state == STATE_INITIALIZING || state == STATE_LIVE) && !finished;
    const progressLabel = error ? 'Error' : `${progressPercentage.toFixed(2)}%`;
    const progressBarVariant = error ? 'danger' : finished ? 'success' : state == STATE_INITIALIZING ? 'warning' : 'primary';

    const formatPeersString = () => {
        let peer_stats = statsResponse?.live?.snapshot.peer_stats;
        if (!peer_stats) {
            return '';
        }
        return `${peer_stats.live} / ${peer_stats.seen}`;
    }

    const formatDownloadSpeed = () => {
        if (finished) {
            return 'Completed';
        }
        switch (state) {
            case STATE_PAUSED: return 'Paused';
            case STATE_INITIALIZING: return 'Checking files';
            case STATE_ERROR: return 'Error';
        }

        return statsResponse.live?.download_speed.human_readable ?? "N/A";
    }

    let classNames = [];

    if (error) {
        classNames.push('bg-warning');
    } else {
        if (id % 2 == 0) {
            classNames.push('bg-light');
        }
    }

    return (
        <Row className={classNames.join(' ')}>
            <Column size={3} label="Name">
                {detailsResponse ?
                    <>
                        <div className='text-truncate'>
                            {getLargestFileName(detailsResponse)}
                        </div>
                        {error && <p className='text-danger'><strong>Error:</strong> {error}</p>}
                    </>
                    : <Spinner />}
            </Column>
            {statsResponse ?
                <>
                    <Column label="Size">{`${formatBytes(totalBytes)} `}</Column>
                    <Column size={2} label={state == STATE_PAUSED ? 'Progress' : 'Progress'}>
                        <ProgressBar
                            now={progressPercentage}
                            label={progressLabel}
                            animated={isAnimated}
                            variant={progressBarVariant} />
                    </Column>
                    <Column size={2} label="Down Speed">{formatDownloadSpeed()}</Column>
                    <Column label="ETA">{getCompletionETA(statsResponse)}</Column>
                    <Column size={2} label="Peers">{formatPeersString()}</Column >
                    <Column label="Actions">
                        <TorrentActions id={id} statsResponse={statsResponse} />
                    </Column>
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
    const [forceStatsRefresh, setForceStatsRefresh] = useState(0);

    const forceStatsRefreshCallback = () => {
        setForceStatsRefresh(forceStatsRefresh + 1);
    }

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
        const liveInterval = 1000;
        const finishedInterval = 10000;
        const nonLiveInterval = 10000;

        return API.getTorrentStats(torrent.id).then((stats) => {
            updateStatsResponse(stats);
            return stats;
        }).then((stats) => {
            if (stats.finished) {
                return finishedInterval;
            }
            if (stats.state == STATE_INITIALIZING || stats.state == STATE_LIVE) {
                return liveInterval;
            }
            return nonLiveInterval;
        }, (e) => {
            return errorInterval;
        });
    }), 0), [forceStatsRefresh]);

    return <RefreshTorrentStatsContext.Provider value={{ refresh: forceStatsRefreshCallback }}>
        <TorrentRow id={id} detailsResponse={detailsResponse} statsResponse={statsResponse} />
    </RefreshTorrentStatsContext.Provider >
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
            <h1 className="mt-3 mb-4">rqbit web 4.0.0-beta.0</h1>
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
    const [listTorrentResponse, setListTorrentResponse] = useState<AddTorrentResponse>(null);
    const [listTorrentError, setListTorrentError] = useState<Error>(null);
    const ctx = useContext(AppContext);

    const showModal = data !== null || listTorrentError !== null;

    // Get the torrent file list if there's data.
    useEffect(() => {
        if (data === null) {
            return;
        }

        let t = setTimeout(async () => {
            setLoading(true);
            try {
                const response = await API.uploadTorrent(data, { listOnly: true });
                setListTorrentResponse(response);
            } catch (e) {
                setListTorrentError({ text: 'Error listing torrent files', details: e });
            } finally {
                setLoading(false);
            }
        }, 0);
        return () => clearTimeout(t);
    }, [data]);

    const clear = () => {
        resetData();
        setListTorrentError(null);
        setListTorrentResponse(null);
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
                listTorrentError={listTorrentError}
                listTorrentResponse={listTorrentResponse}
                data={data}
                listTorrentLoading={loading}
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
        <UploadButton variant='primary' buttonText="Add Torrent from Magnet / URL" onClick={onClick} data={magnet} resetData={() => setMagnet(null)} />
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
    listTorrentResponse: AddTorrentResponse,
    listTorrentError: Error,
    listTorrentLoading: boolean,
    data: string | File
}) => {
    let { show, onHide, listTorrentResponse, listTorrentError, listTorrentLoading, data } = props;

    const [selectedFiles, setSelectedFiles] = useState([]);
    const [uploading, setUploading] = useState(false);
    const [uploadError, setUploadError] = useState<Error>(null);
    const [unpopularTorrent, setUnpopularTorrent] = useState(false);
    const ctx = useContext(AppContext);

    useEffect(() => {
        setSelectedFiles(listTorrentResponse ? listTorrentResponse.details.files.map((_, id) => id) : []);
    }, [listTorrentResponse]);

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
        let initialPeers = listTorrentResponse.seen_peers ? listTorrentResponse.seen_peers.slice(0, 32) : null;
        API.uploadTorrent(data, { selectedFiles, unpopularTorrent, initialPeers }).then(() => {
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
                <Modal.Title>Add torrent</Modal.Title>
            </Modal.Header>
            <Modal.Body>
                <Form>
                    <fieldset className='mb-5'>
                        <legend>Pick the files to download</legend>
                        {listTorrentLoading ? <Spinner />
                            : listTorrentError ? <ErrorComponent error={listTorrentError}></ErrorComponent> :
                                <>
                                    {listTorrentResponse?.details.files.map((file, index) => (
                                        <Form.Group key={index} controlId={`check-${index}`}>
                                            <Form.Check
                                                type="checkbox"
                                                label={`${file.name}  (${formatBytes(file.length)})`}
                                                checked={selectedFiles.includes(index)}
                                                onChange={() => handleToggleFile(index)}>
                                            </Form.Check>
                                        </Form.Group>
                                    ))}
                                </>
                        }
                    </fieldset>
                    <fieldset>
                        <legend>Other options</legend>

                        <Form.Group controlId='unpopular-torrent'>
                            <Form.Check
                                type="checkbox"
                                label="Increase timeouts"
                                checked={unpopularTorrent}
                                onChange={() => setUnpopularTorrent(!unpopularTorrent)}>
                            </Form.Check>
                            <small id="emailHelp" className="form-text text-muted">This might be useful for unpopular torrents with few peers.</small>
                        </Form.Group>
                    </fieldset>

                </Form>
                <ErrorComponent error={uploadError} />
            </Modal.Body>
            <Modal.Footer>
                {uploading && <Spinner />}
                <Button variant="primary" onClick={handleUpload} disabled={listTorrentLoading || uploading || selectedFiles.length == 0}>
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
    let duration = stats?.live?.time_remaining?.duration?.secs;
    if (duration == null) {
        return 'N/A';
    }
    return formatSecondsToTime(duration);
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