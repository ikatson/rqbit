// Define API URL and base path
const apiUrl = window.origin == 'null' ? 'http://localhost:3030' : '';

// Helper function for making API requests (async/await)
async function makeRequest(method, path, data) {
    const url = apiUrl + path;
    const options = {
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
            }
            catch (e) {
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
    }
    catch (error) {
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
function displayResult(result) {
    const outputDiv = document.getElementById('output');
    if (outputDiv) {
        outputDiv.innerHTML = `<pre>${result}</pre>`;
    }
}
// Function to get detailed information about a torrent (async/await)
async function getTorrentDetails(index) {
    return makeRequest('GET', `/torrents/${index}`);
}
// Function to get detailed statistics about a torrent (async/await)
async function getTorrentStats(index) {
    return makeRequest('GET', `/torrents/${index}/stats`);
}
// Display function for listing all torrents with concise information (async/await)
async function displayTorrents() {
    try {
        const response = await makeRequest('GET', '/torrents');
        const torrents = response.torrents;
        // Create a container for all torrents using Bootstrap classes
        const torrentsContainer = document.createElement('div');
        torrentsContainer.classList.add('d-flex', 'flex-column', 'torrents-container');
        for (const torrent of torrents) {
            const detailsResponse = await getTorrentDetails(torrent.id);
            const statsResponse = await getTorrentStats(torrent.id);
            const totalBytes = detailsResponse.files.reduce((total, file) => total + file.length, 0);
            const downloadedBytes = statsResponse.snapshot.have_bytes;
            // Calculate download percentage
            const downloadPercentage = (downloadedBytes / totalBytes) * 100;
            // Create a container for each torrent using Bootstrap classes
            const torrentContainer = document.createElement('div');
            torrentContainer.classList.add('torrent-container', 'd-flex', 'flex-row', 'p-3', 'bg-light', 'rounded', 'mb-3');
            // Display basic information about the torrent
            const largestFileName = getLargestFileName(detailsResponse);
            const downloadSpeed = statsResponse.download_speed.human_readable;
            const eta = getCompletionETA(statsResponse);
            // Create and append divs for concise information as columns
            const nameColumn = createColumn('Name', largestFileName);
            const sizeColumn = createColumn('Size', `${formatBytesToGB(totalBytes)} GB`);
            const progressColumn = createColumnWithProgressBar('Progress', downloadPercentage);
            const downloadSpeedColumn = createColumn('Download Speed', downloadSpeed);
            const etaColumn = createColumn('ETA', eta);
            // Append columns to the torrent container
            torrentContainer.appendChild(nameColumn);
            torrentContainer.appendChild(sizeColumn);
            torrentContainer.appendChild(progressColumn);
            torrentContainer.appendChild(downloadSpeedColumn);
            torrentContainer.appendChild(etaColumn);
            // Append the torrent container to the torrentsContainer
            torrentsContainer.appendChild(torrentContainer);
        }
        // Replace the old content with the new one
        const outputDiv = document.getElementById('output');
        outputDiv.replaceChildren(torrentsContainer);
    }
    catch (error) {
        console.error(error);
    }
}
// Function to create a column div
function createColumn(label, value) {
    const columnDiv = document.createElement('div');
    columnDiv.classList.add('me-3', 'p-2');
    columnDiv.innerHTML = `<p class="font-weight-bold">${label}</p><p>${value}</p>`;
    return columnDiv;
}
// Function to create a column div with a progress bar
function createColumnWithProgressBar(label, percentage) {
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
function formatBytesToGB(bytes) {
    const GB = bytes / (1024 * 1024 * 1024);
    return GB.toFixed(2);
}
// Function to get the name of the largest file in a torrent
function getLargestFileName(torrentDetails) {
    const largestFile = torrentDetails.files.reduce((prev, current) => (prev.length > current.length) ? prev : current);
    return largestFile.name;
}
// Function to get the completion ETA of a torrent
function getCompletionETA(stats) {
    if (stats.time_remaining) {
        return stats.time_remaining.human_readable;
    }
    else {
        return 'N/A';
    }
}
// Helper function to display API errors in an alert
function displayApiError(error) {
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
function clearErrorAlert() {
    const errorAlert = document.getElementById('error-alert');
    if (errorAlert) {
        errorAlert.innerHTML = ''; // Clear the content
    }
}
// List all torrents on page load and set up auto-refresh
async function init() {
    try {
        await displayTorrents();
        autoRefreshTorrents(500); // Set the interval (in milliseconds), e.g., 5000 for every 5 seconds
    }
    catch (error) {
        console.error(error);
    }
}
// Function to refresh torrents at a specified interval
function autoRefreshTorrents(interval) {
    setInterval(async () => {
        await displayTorrents();
    }, interval);
}
// Function to add a torrent from a magnet link
async function addTorrentFromMagnet() {
    const magnetLink = prompt('Enter magnet link:');
    if (magnetLink) {
        await makeRequest('POST', '/torrents?overwrite=true', magnetLink);
        await displayTorrents(); // Refresh the torrent list after adding a new torrent
    }
}
// Function to handle file input change
async function handleFileInputChange() {
    const fileInput = document.getElementById('file-input');
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
    const fileInput = document.getElementById('file-input');
    fileInput.click();
});
document.getElementById('file-input')?.addEventListener('change', handleFileInputChange);
// Call init function on page load
document.addEventListener('DOMContentLoaded', init);
