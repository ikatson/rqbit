import { MouseEventHandler, RefObject, createContext, useContext, useEffect, useRef, useState } from 'react';
import { ProgressBar, Button, Container, Row, Col, Alert, Modal, Form, Spinner } from 'react-bootstrap';
import { AddTorrentResponse, TorrentDetails, TorrentId, TorrentStats, ErrorDetails as ApiErrorDetails, STATE_INITIALIZING, STATE_LIVE, STATE_PAUSED, STATE_ERROR, RqbitAPI, AddTorrentOptions } from './api-types';

interface Error {
    text: string,
    details?: ApiErrorDetails,
}

interface ContextType {
    setCloseableError: (error: Error | null) => void,
    refreshTorrents: () => void,
}

export const APIContext = createContext<RqbitAPI>({
    listTorrents: () => {
        throw new Error('Function not implemented.');
    },
    getTorrentDetails: () => {
        throw new Error('Function not implemented.');
    },
    getTorrentStats: () => {
        throw new Error('Function not implemented.');
    },
    uploadTorrent: () => {
        throw new Error('Function not implemented.');
    },
    pause: () => {
        throw new Error('Function not implemented.');
    },
    start: () => {
        throw new Error('Function not implemented.');
    },
    forget: () => {
        throw new Error('Function not implemented.');
    },
    delete: () => {
        throw new Error('Function not implemented.');
    }
});

const AppContext = createContext<ContextType>({
    setCloseableError: (_) => { },
    refreshTorrents: () => { },
});
const RefreshTorrentStatsContext = createContext({ refresh: () => { } });

const IconButton: React.FC<{
    className: string,
    onClick: () => void,
    disabled?: boolean,
    color?: string,
}> = ({ className, onClick, disabled, color }) => {
    const onClickStopPropagation: MouseEventHandler<HTMLAnchorElement> = (e) => {
        e.stopPropagation();
        if (disabled) {
            return;
        }
        onClick();
    }
    return <a className={`bi ${className} p-1`} onClick={onClickStopPropagation} href='#'></a>
}

const DeleteTorrentModal: React.FC<{
    id: number,
    show: boolean,
    onHide: () => void
}> = ({ id, show, onHide }) => {
    if (!show) {
        return null;
    }
    const [deleteFiles, setDeleteFiles] = useState(false);
    const [error, setError] = useState<Error | null>(null);
    const [deleting, setDeleting] = useState(false);

    const ctx = useContext(AppContext);
    const API = useContext(APIContext);

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
    const API = useContext(APIContext);

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
    id: number,
    detailsResponse: TorrentDetails | null,
    statsResponse: TorrentStats | null
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

        return statsResponse?.live?.download_speed.human_readable ?? "N/A";
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

const Torrent: React.FC<{
    id: number,
    torrent: TorrentId
}> = ({ id, torrent }) => {
    const [detailsResponse, updateDetailsResponse] = useState<TorrentDetails | null>(null);
    const [statsResponse, updateStatsResponse] = useState<TorrentStats | null>(null);
    const [forceStatsRefresh, setForceStatsRefresh] = useState(0);
    const API = useContext(APIContext);

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
        }, () => {
            return errorInterval;
        });
    }), 0), [forceStatsRefresh]);

    return <RefreshTorrentStatsContext.Provider value={{ refresh: forceStatsRefreshCallback }}>
        <TorrentRow id={id} detailsResponse={detailsResponse} statsResponse={statsResponse} />
    </RefreshTorrentStatsContext.Provider >
}

const TorrentsList = (props: { torrents: Array<TorrentId> | null, loading: boolean }) => {
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

export const RqbitWebUI = (props: { title: string }) => {
    const [closeableError, setCloseableError] = useState<Error | null>(null);
    const [otherError, setOtherError] = useState<Error | null>(null);

    const [torrents, setTorrents] = useState<Array<TorrentId> | null>(null);
    const [torrentsLoading, setTorrentsLoading] = useState(false);
    const API = useContext(APIContext);

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
            <h1 className="mt-3 mb-4">{props.title}</h1>
            <RootContent
                closeableError={closeableError}
                otherError={otherError}
                torrents={torrents}
                torrentsLoading={torrentsLoading} />
        </div>
    </AppContext.Provider >
}

const ErrorDetails = (props: { details: ApiErrorDetails | null | undefined }) => {
    let { details } = props;
    if (!details) {
        return null;
    }
    return <>
        {details.statusText && <p><strong>{details.statusText}</strong></p>}
        <pre>{details.text}</pre>
    </>

}

const ErrorComponent = (props: { error: Error | null, remove?: () => void }) => {
    let { error, remove } = props;

    if (error == null) {
        return null;
    }

    return (<Alert variant='danger' onClose={remove} dismissible={remove != null}>
        <Alert.Heading>{error.text}</Alert.Heading>

        <ErrorDetails details={error.details} />
    </Alert>);
};

const UploadButton: React.FC<{
    buttonText: string,
    onClick: () => void,
    data: string | File | null,
    resetData: () => void,
    variant: string,
}> = ({ buttonText, onClick, data, resetData, variant }) => {
    const [loading, setLoading] = useState(false);
    const [listTorrentResponse, setListTorrentResponse] = useState<AddTorrentResponse | null>(null);
    const [listTorrentError, setListTorrentError] = useState<Error | null>(null);
    const API = useContext(APIContext);

    // Get the torrent file list if there's data.
    useEffect(() => {
        if (data === null) {
            return;
        }

        let t = setTimeout(async () => {
            setLoading(true);
            try {
                const response = await API.uploadTorrent(data, { list_only: true });
                setListTorrentResponse(response);
            } catch (e) {
                setListTorrentError({ text: 'Error listing torrent files', details: e as ApiErrorDetails });
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

            {data && <FileSelectionModal
                onHide={clear}
                listTorrentError={listTorrentError}
                listTorrentResponse={listTorrentResponse}
                data={data}
                listTorrentLoading={loading}
            />}
        </>
    );
};

const UrlPromptModal: React.FC<{
    show: boolean,
    setUrl: (_: string) => void,
    cancel: () => void,
}> = ({ show, setUrl, cancel }) => {
    let [inputValue, setInputValue] = useState('');
    return <Modal show={show} onHide={cancel} size='lg'>
        <Modal.Header closeButton>
            <Modal.Title>Add torrent</Modal.Title>
        </Modal.Header>
        <Modal.Body>
            <Form>
                <Form.Group className="mb-3" controlId="url">
                    <Form.Label>Enter magnet or HTTP(S) URL to the .torrent</Form.Label>
                    <Form.Control value={inputValue} placeholder="magnet:?xt=urn:btih:..." onChange={(u) => { setInputValue(u.target.value) }} />
                </Form.Group>
            </Form>
        </Modal.Body>
        <Modal.Footer>
            <Button
                variant="primary"
                onClick={() => { setUrl(inputValue); setInputValue(''); }}
                disabled={inputValue.length == 0}>
                OK
            </Button>
            <Button variant="secondary" onClick={cancel}>
                Cancel
            </Button>
        </Modal.Footer>
    </Modal >
}

const MagnetInput = () => {
    let [magnet, setMagnet] = useState<string | null>(null);

    let [showModal, setShowModal] = useState(false);

    return (
        <>
            <UploadButton
                variant='primary'
                buttonText="Add Torrent from Magnet / URL"
                onClick={() => {
                    setShowModal(true);
                }}
                data={magnet}
                resetData={() => setMagnet(null)}
            />

            <UrlPromptModal
                show={showModal}
                setUrl={(url) => {
                    setShowModal(false);
                    setMagnet(url);
                }}
                cancel={() => {
                    setShowModal(false);
                    setMagnet(null);
                }} />
        </>
    );
};

const FileInput = () => {
    const inputRef = useRef<HTMLInputElement>() as RefObject<HTMLInputElement>;
    const [file, setFile] = useState<File | null>(null);

    const onFileChange = async () => {
        if (!inputRef?.current?.files) {
            return;
        }
        const file = inputRef.current.files[0];
        setFile(file);
    };

    const reset = () => {
        if (!inputRef?.current) {
            return;
        }
        inputRef.current.value = '';
        setFile(null);
    }

    const onClick = () => {
        if (!inputRef?.current) {
            return;
        }
        inputRef.current.click();
    }

    return (
        <>
            <input type="file" ref={inputRef} accept=".torrent" onChange={onFileChange} className='d-none' />
            <UploadButton
                variant='secondary'
                buttonText="Upload .torrent File"
                onClick={onClick}
                data={file}
                resetData={reset}
            />
        </>
    );
};

const FileSelectionModal = (props: {
    onHide: () => void,
    listTorrentResponse: AddTorrentResponse | null,
    listTorrentError: Error | null,
    listTorrentLoading: boolean,
    data: string | File,
}) => {
    let { onHide, listTorrentResponse, listTorrentError, listTorrentLoading, data } = props;

    const [selectedFiles, setSelectedFiles] = useState<number[]>([]);
    const [uploading, setUploading] = useState(false);
    const [uploadError, setUploadError] = useState<Error | null>(null);
    const [unpopularTorrent, setUnpopularTorrent] = useState(false);
    const [outputFolder, setOutputFolder] = useState<string>('');
    const ctx = useContext(AppContext);
    const API = useContext(APIContext);

    useEffect(() => {
        console.log(listTorrentResponse);
        setSelectedFiles(listTorrentResponse ? listTorrentResponse.details.files.map((_, id) => id) : []);
        setOutputFolder(listTorrentResponse?.output_folder || '');
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
        if (!listTorrentResponse) {
            return;
        }
        setUploading(true);
        let initialPeers = listTorrentResponse.seen_peers ? listTorrentResponse.seen_peers.slice(0, 32) : null;
        let opts: AddTorrentOptions = {
            overwrite: true,
            only_files: selectedFiles,
            initial_peers: initialPeers,
            output_folder: outputFolder,
        };
        if (unpopularTorrent) {
            opts.peer_opts = {
                connect_timeout: 20,
                read_write_timeout: 60,
            };
        }
        API.uploadTorrent(data, opts).then(() => {
            onHide();
            ctx.refreshTorrents();
        },
            (e) => {
                setUploadError({ text: 'Error starting torrent', details: e });
            }
        ).finally(() => setUploading(false));
    };

    const getBody = () => {
        if (listTorrentLoading) {
            return <Spinner />;
        } else if (listTorrentError) {
            return <ErrorComponent error={listTorrentError}></ErrorComponent>;
        } else if (listTorrentResponse) {
            return <Form>
                <fieldset className='mb-4'>
                    <legend>Pick the files to download</legend>
                    {listTorrentResponse.details.files.map((file, index) => (
                        <Form.Group key={index} controlId={`check-${index}`}>
                            <Form.Check
                                type="checkbox"
                                label={`${file.name}  (${formatBytes(file.length)})`}
                                checked={selectedFiles.includes(index)}
                                onChange={() => handleToggleFile(index)}>
                            </Form.Check>
                        </Form.Group>
                    ))}
                </fieldset>
                <fieldset>
                    <legend>Options</legend>
                    <Form.Group controlId='output-folder' className="mb-3">
                        <Form.Label>Output folder</Form.Label>
                        <Form.Control
                            type="text"
                            value={outputFolder}
                            onChange={(e) => setOutputFolder(e.target.value)}
                        />
                    </Form.Group>
                    <Form.Group controlId='unpopular-torrent' className="mb-3">
                        <Form.Check
                            type="checkbox"
                            label="Increase timeouts"
                            checked={unpopularTorrent}
                            onChange={() => setUnpopularTorrent(!unpopularTorrent)}>
                        </Form.Check>
                        <small id="emailHelp" className="form-text text-muted">This might be useful for unpopular torrents with few peers. It will slow down fast torrents though.</small>
                    </Form.Group>
                </fieldset>
            </Form>
        }
    };

    return (
        <Modal show onHide={clear} size='lg'>
            <Modal.Header closeButton>
                <Modal.Title>Add torrent</Modal.Title>
            </Modal.Header>
            <Modal.Body>
                {getBody()}
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

const RootContent = (props: {
    closeableError: ApiErrorDetails | null,
    otherError: ApiErrorDetails | null,
    torrents: Array<TorrentId> | null,
    torrentsLoading: boolean
}) => {
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