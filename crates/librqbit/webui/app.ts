// Define API URL and base path
const apiUrl = window.origin == 'null' ? 'http://localhost:3030' : '';

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
        remaining_bytes: number;
    };
    download_speed: {
        mbps: number;
        human_readable: string;
    };
    time_remaining?: {
        human_readable: string;
    };
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

// Helper function to display the API response
function displayResult(result: string): void {
    const outputDiv = document.getElementById('output');
    if (outputDiv) {
        outputDiv.innerHTML = `<pre>${result}</pre>`;
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

// Display function for listing all torrents with concise information (async/await)
async function displayTorrents() {
    try {
        const response = await makeRequest('GET', '/torrents');
        const torrents = response.torrents;

        // Create a container for all torrents using Bootstrap classes
        const torrentsContainer = document.createElement('div');
        torrentsContainer.id = 'torrents-container';
        torrentsContainer.classList.add('d-flex', 'flex-column', 'torrents-container');

        // Map to store existing elements
        const existingElementsMap = new Map();

        // Array to hold promises for torrent details and stats
        const promises = torrents.map(async (torrent) => {
            const detailsPromise = getTorrentDetails(torrent.id);
            const statsPromise = getTorrentStats(torrent.id);
            const [detailsResponse, statsResponse] = await Promise.all([detailsPromise, statsPromise]);

            const totalBytes = detailsResponse.files.reduce((total, file) => total + file.length, 0);
            const downloadedBytes = statsResponse.snapshot.have_bytes;

            // Calculate download percentage
            const downloadPercentage = (downloadedBytes / totalBytes) * 100;

            // Check if the torrent container already exists
            let torrentContainer = existingElementsMap.get(torrent.id);

            if (!torrentContainer) {
                torrentContainer = document.createElement('div');
                torrentContainer.id = `torrent-${torrent.id}`;
                torrentContainer.classList.add('torrent-container', 'd-flex', 'flex-row', 'p-3', 'bg-light', 'rounded', 'mb-3');
                existingElementsMap.set(torrent.id, torrentContainer);
            }

            // Display basic information about the torrent
            const largestFileName = getLargestFileName(detailsResponse);
            const downloadSpeed = statsResponse.download_speed.human_readable;
            const eta = getCompletionETA(statsResponse);

            // Update or append columns to the torrent container
            torrentContainer.innerHTML = ''; // Clear existing content
            torrentContainer.appendChild(createColumn('Name', largestFileName, 'name-column'));
            torrentContainer.appendChild(createColumn('Size', `${formatBytesToGB(totalBytes)} GB`, 'size-column'));
            torrentContainer.appendChild(createColumnWithProgressBar('Progress', downloadPercentage, 'progress-column'));
            torrentContainer.appendChild(createColumn('Download Speed', downloadSpeed, 'download-speed-column'));
            torrentContainer.appendChild(createColumn('ETA', eta, 'eta-column'));

            // Append the torrent container to the torrentsContainer
            torrentsContainer.appendChild(torrentContainer);
        });

        // Replace the old content with loading spinners
        const outputDiv = document.getElementById('output') || document.createElement('div');
        outputDiv.id = 'output';
        outputDiv.innerHTML = `<div class="spinner-border" role="status"><span class="visually-hidden">Loading...</span></div>`;

        // Wait for all promises to resolve
        await Promise.all(promises);

        // Replace the loading spinners with the new content
        outputDiv.replaceChildren(torrentsContainer);
    } catch (error) {
        console.error(error);
        // Handle errors as needed
    }
}


// Function to create a column div
function createColumn(label: string, value: string): HTMLDivElement {
    const columnDiv = document.createElement('div');
    columnDiv.classList.add('me-3', 'p-2');
    columnDiv.innerHTML = `<p class="font-weight-bold">${label}</p><p>${value}</p>`;
    return columnDiv;
}

// Function to create a column div with a progress bar
function createColumnWithProgressBar(label: string, percentage: number): HTMLDivElement {
    const columnDiv = document.createElement('div');
    columnDiv.classList.add('column', 'me-3', 'p-2');
    columnDiv.innerHTML = `
        <p class="font-weight-bold">${label}</p>
        <div class="progress mb-1">
            <div class="progress-bar" role="progressbar" style="width: ${percentage}%;"></div>
        </div>
        <p class="mb-1">${percentage.toFixed(2)}%</p>`;
    return columnDiv;
}

// Function to format bytes to GB
function formatBytesToGB(bytes: number): string {
    const GB = bytes / (1024 * 1024 * 1024);
    return GB.toFixed(2);
}

// Function to get the name of the largest file in a torrent
function getLargestFileName(torrentDetails: TorrentDetails): string {
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
    const errorAlert = document.getElementById('error-alert');
    if (errorAlert) {
        errorAlert.innerHTML = `
            <div class="alert alert-danger alert-dismissible fade show" role="alert">
                <strong>Error ${error.status}:</strong> ${error.statusText}<br>
                ${error.body}
                <button type="button" class="btn-close" data-bs-dismiss="alert" aria-label="Close" onclick="clearErrorAlert()"></button>
            </div>
        `;
    }
}

// Helper function to clear the error alert
function clearErrorAlert(): void {
    const errorAlert = document.getElementById('error-alert');
    if (errorAlert) {
        errorAlert.innerHTML = ''; // Clear the content
    }
}

// List all torrents on page load and set up auto-refresh
async function init(): Promise<void> {
    try {
        await displayTorrents();
        autoRefreshTorrents(500); // Set the interval (in milliseconds), e.g., 5000 for every 5 seconds
    } catch (error) {
        console.error(error);
    }
}

// Function to refresh torrents at a specified interval
function autoRefreshTorrents(interval: number): void {
    setInterval(async () => {
        await displayTorrents();
    }, interval);
}

// Function to add a torrent from a magnet link
async function addTorrentFromMagnet(): Promise<void> {
    const magnetLink = prompt('Enter magnet link:');
    if (magnetLink) {
        await makeRequest('POST', '/torrents?overwrite=true', magnetLink);
        await displayTorrents(); // Refresh the torrent list after adding a new torrent
    }
}

// Function to handle file input change
async function handleFileInputChange(): Promise<void> {
    const fileInput = document.getElementById('file-input') as HTMLInputElement;
    const file = fileInput.files?.[0];
    if (file) {
        await makeRequest('POST', '/torrents?overwrite=true', file);
        await displayTorrents(); // Refresh the torrent list after adding a new torrent
    }
}

// Add event listeners for buttons
document.getElementById('add-magnet-button')?.addEventListener('click', addTorrentFromMagnet);

// Update the event listener for the file input button
const fileInputButton = document.getElementById('upload-file-button');
fileInputButton?.addEventListener('click', () => {
    const fileInput = document.getElementById('file-input') as HTMLInputElement;
    fileInput.click();
});

document.getElementById('file-input')?.addEventListener('change', handleFileInputChange);

// Call init function on page load
document.addEventListener('DOMContentLoaded', init);
