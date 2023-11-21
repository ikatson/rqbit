import { Fragment, createContext, useContext, useEffect, useRef, useState } from 'react';
import * as ReactDOM from 'react-dom';

// Define API URL and base path
const apiUrl = (window.origin === 'null' || window.origin === 'http://localhost:3031') ? 'http://localhost:3030' : '';

interface ErrorType {
	status: number,
	statusText: number,
	body: number,
};

let defaultContext: {
	setError: any,
} = {
	setError: null,
};

const AppContext = createContext(defaultContext);

// Interface for the Torrent API response
interface Torrent {
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


// Interface for the API error response
interface ApiError {
	status: number;
	statusText: string;
	body: string;
}

// Helper function for making API requests (async/await)
async function makeRequest(method: string, path: string, data?: any): Promise<any> {
	const url = apiUrl + path;
	const options: RequestInit = {
		method,
		headers: {
			'Accept': 'application/json',
		},
		body: data,
	};

	try {
		const response = await fetch(url, options);
		if (!response.ok) {
			const errorBody = await response.text();
			try {
				const json = JSON.parse(errorBody);
				displayApiError({
					status: response.status,
					statusText: response.statusText,
					body: json.human_readable !== undefined ? json.human_readable : errorBody,
				});
			} catch (e) {
				displayApiError({
					status: response.status,
					statusText: response.statusText,
					body: errorBody,
				});
			}
			return Promise.reject(errorBody);
		}
		const result = await response.json();
		return result;
	} catch (error) {
		console.error(error);
		displayApiError({
			status: error.status,
			statusText: error.statusText,
			body: error.toString(),
		});
		return Promise.reject(`Error: ${error.message}`);
	}
}

// Function to get detailed information about a torrent (async/await)
async function getTorrentDetails(index: number): Promise<TorrentDetails> {
	return makeRequest('GET', `/torrents/${index}`);
}

// Function to get detailed statistics about a torrent (async/await)
async function getTorrentStats(index: number): Promise<TorrentStats> {
	return makeRequest('GET', `/torrents/${index}/stats`);
}

function TorrentRow({ detailsResponse, statsResponse }) {
	const totalBytes = statsResponse.snapshot.total_bytes;
	const downloadedBytes = statsResponse.snapshot.have_bytes;
	const downloadPercentage = (downloadedBytes / totalBytes) * 100;

	return (
		<div className="torrent-row d-flex flex-row p-3 bg-light rounded mb-3">
			{/* Create and render columns */}
			<Column label="Name" value={getLargestFileName(detailsResponse)} />
			<Column label="Size" value={`${formatBytesToGB(totalBytes)} GB`} />
			<ColumnWithProgressBar label="Progress" percentage={downloadPercentage} />
			<Column label="Download Speed" value={statsResponse.download_speed.human_readable} />
			<Column label="ETA" value={getCompletionETA(statsResponse)} />
			<Column label="Peers" value={`${statsResponse.snapshot.peer_stats.live} / ${statsResponse.snapshot.peer_stats.seen}`} />
		</div>
	);
}

// Define a Preact component for a column
const Column = ({ label, value }) => (
	<div className={`column-${label.toLowerCase().replace(" ", "-")} me-3 p-2`}>
		<p className="font-weight-bold">{label}</p>
		<p>{value}</p>
	</div>
);

// Define a Preact component for a column with a progress bar
const ColumnWithProgressBar = ({ label, percentage }) => (
	<div className="column-progress me-3 p-2">
		<p className="font-weight-bold">{label}</p>
		<div className="progress mb-1">
			<div className="progress-bar" style={{ width: `${percentage}%` }}></div>
		</div>
		<p className="mb-1">{percentage.toFixed(2)}%</p>
	</div >
);

const DeferredTorrent = ({ torrent }) => {
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

	const update = async () => {
		let a = getTorrentDetails(torrent.id).then((details) => {
			updateDetailsResponse(details);
		});
		let b = getTorrentStats(torrent.id).then((stats) => {
			updateStatsResponse(stats);
		});
		await Promise.all([a, b]);
		setTimeout(update, 500);
	};

	useEffect(() => {
		let timer = setTimeout(update, 0);
		return () => clearTimeout(timer);
	}, []);

	return <TorrentRow detailsResponse={detailsResponse} statsResponse={statsResponse} />
}

var globalCtx = null;

const TorrentsList = () => {
	const [torrents, updateTorrents] = useState([]);
	globalCtx = useContext(AppContext);
	const update = async () => {
		let response = await makeRequest('GET', '/torrents');
		updateTorrents(response.torrents);
		setTimeout(update, 500);
	};

	useEffect(() => {
		let timer = setTimeout(update, 0);
		return () => clearTimeout(timer);
	}, []);

	let torrentsComponents = torrents.map((t: Torrent) =>
		<DeferredTorrent key={t.id} torrent={t} />
	);

	return (
		<div>
			{torrentsComponents}
		</div>
	)
};

const Root = () => {
	const [error, setError] = useState(null);

	const Error = ({ error }) => {
		if (error == null) {
			return null;
		}

		let ctx = useContext(AppContext);

		return (<div className="alert alert-danger alert-dismissible fade show" role="alert">
			<strong>Error ${error.status}:</strong> {error.statusText}<br />
			{error.body}
			<button type="button" className="btn-close" data-bs-dismiss="alert" aria-label="Close" onClick={() => ctx.setError(null)}>Close</button>
		</div>);
	};

	const FileInput = () => {
		const inputRef = useRef();

		const inputOnChange = (e) => {
			let file = e.target.files[0];
			makeRequest('POST', '/torrents?overwrite=true', file);
			//			e.target.clear();
		}

		const onClick = (e) => {
			inputRef.current.click();
		}

		return (<div>
			<input type="file" ref={inputRef} id="file-input" accept=".torrent" onChange={inputOnChange} style={{ display: 'none' }} />
			<button id="upload-file-button" className="btn btn-secondary" onClick={onClick}>Upload .torrent File</button>
		</div>);
	};

	const Buttons = () => {
		return (
			<div id="buttons-container" className="mt-3">
				<button id="add-magnet-button" className="btn btn-primary mr-2" onClick={addTorrentFromMagnet}>Add Torrent from Magnet Link</button>
				<FileInput />
			</div>
		);
	}

	return <AppContext.Provider value={{ setError: setError }}>
		<Error error={error} />
		<TorrentsList />
		<Buttons />
	</AppContext.Provider >
}

// Render function to display all torrents
async function displayTorrents() {
	// Get the torrents container
	const torrentsContainer = document.getElementById('output');
	ReactDOM.render(<Root />, torrentsContainer);
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

// Helper function to display API errors in an alert
function displayApiError(error: ApiError): void {
	globalCtx.setError(error);
}

// List all torrents on page load and set up auto-refresh
async function init(): Promise<void> {
	await displayTorrents();
}

// Function to add a torrent from a magnet link
async function addTorrentFromMagnet(): Promise<void> {
	const magnetLink = prompt('Enter magnet link:');
	if (magnetLink) {
		await makeRequest('POST', '/torrents?overwrite=true', magnetLink);
		//		await displayTorrents(); // Refresh the torrent list after adding a new torrent
	}
}

// Function to handle file input change
async function handleFileInputChange(): Promise<void> {
	const fileInput = document.getElementById('file-input') as HTMLInputElement;
	const file = fileInput.files?.[0];
	if (file) {
		await makeRequest('POST', '/torrents?overwrite=true', file);
		//		await displayTorrents(); // Refresh the torrent list after adding a new torrent
	}
}

// Update the event listener for the file input button
const fileInputButton = document.getElementById('upload-file-button');
fileInputButton?.addEventListener('click', () => {
	const fileInput = document.getElementById('file-input') as HTMLInputElement;
	fileInput.click();
});

document.getElementById('file-input')?.addEventListener('change', handleFileInputChange);

// Call init function on page load
document.addEventListener('DOMContentLoaded', init);