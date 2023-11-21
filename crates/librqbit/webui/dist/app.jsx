"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
const preact_1 = require("preact");
// Define API URL and base path
const apiUrl = (window.origin === 'null' || window.origin === 'http://localhost:8000') ? 'http://localhost:3030' : '';
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
// Function to get detailed information about a torrent (async/await)
async function getTorrentDetails(index) {
    return makeRequest('GET', `/torrents/${index}`);
}
// Function to get detailed statistics about a torrent (async/await)
async function getTorrentStats(index) {
    return makeRequest('GET', `/torrents/${index}/stats`);
}
function TorrentRow(props) {
    const { detailsResponse, statsResponse } = this.props;
    const totalBytes = statsResponse.snapshot.total_bytes;
    const downloadedBytes = statsResponse.snapshot.have_bytes;
    const downloadPercentage = (downloadedBytes / totalBytes) * 100;
    return (<div class="torrent-row d-flex flex-row p-3 bg-light rounded mb-3">
            {/* Create and render columns */}
            <Column label="Name" value={getLargestFileName(detailsResponse)}/>
            <Column label="Size" value={`${formatBytesToGB(totalBytes)} GB`}/>
            <ColumnWithProgressBar label="Progress" percentage={downloadPercentage}/>
            <Column label="Download Speed" value={statsResponse.download_speed.human_readable}/>
            <Column label="ETA" value={getCompletionETA(statsResponse)}/>
            <Column label="Peers" value={`${statsResponse.snapshot.peer_stats.live} / ${statsResponse.snapshot.peer_stats.seen}`}/>
        </div>);
}
// Define a Preact component for a column
const Column = ({ label, value }) => (<div class={`column-${label.toLowerCase().replace(" ", "-")} me-3 p-2`}>
        <p class="font-weight-bold">{label}</p>
        <p>{value}</p>
    </div>);
// Define a Preact component for a column with a progress bar
const ColumnWithProgressBar = ({ label, percentage }) => (<div class="column-progress me-3 p-2">
        <p class="font-weight-bold">{label}</p>
        <div class="progress mb-1">
            <div class="progress-bar" style={`width: ${percentage}%;`}></div>
        </div>
        <p class="mb-1">{percentage.toFixed(2)}%</p>
    </div>);
// Render function to display all torrents
async function displayTorrents() {
    try {
        const response = await makeRequest('GET', '/torrents');
        const torrents = response.torrents;
        // Get the torrents container
        const torrentsContainer = document.getElementById('output');
        // Render each torrent using the TorrentRow component
        let children = torrents.map(async (torrent) => {
            const detailsResponse = await getTorrentDetails(torrent.id);
            const statsResponse = await getTorrentStats(torrent.id);
            <TorrentRow detailsResponse={detailsResponse} statsResponse={statsResponse}/>;
        });
        (0, preact_1.render)(children, torrentsContainer);
    }
    catch (error) {
        console.error(error);
        // Handle errors as needed
    }
}
// Function to update HTML for a torrent row
function updateTorrentRow(torrentRow, detailsResponse, statsResponse) {
    // Calculate download percentage
    const totalBytes = statsResponse.snapshot.total_bytes;
    const downloadedBytes = statsResponse.snapshot.have_bytes;
    const downloadPercentage = (downloadedBytes / totalBytes) * 100;
    // Display basic information about the torrent
    const largestFileName = getLargestFileName(detailsResponse);
    const downloadSpeed = statsResponse.download_speed.human_readable;
    const eta = getCompletionETA(statsResponse);
    const peers = `${statsResponse.snapshot.peer_stats.live} / ${statsResponse.snapshot.peer_stats.seen}`;
    // Update or create columns in the torrent row
    updateOrCreateColumnContent(torrentRow, 'Name', largestFileName);
    updateOrCreateColumnContent(torrentRow, 'Size', `${formatBytesToGB(totalBytes)} GB`);
    updateOrCreateColumnWithProgressBar(torrentRow, 'Progress', downloadPercentage);
    updateOrCreateColumnContent(torrentRow, 'Download Speed', downloadSpeed);
    updateOrCreateColumnContent(torrentRow, 'ETA', eta);
    updateOrCreateColumnContent(torrentRow, 'Peers', peers);
}
// Function to update or create the content of a column in a torrent row
function updateOrCreateColumnContent(torrentRow, human_label, value) {
    let label = human_label.toLowerCase().replace(" ", "-");
    let column = torrentRow.querySelector(`.column-${label}`);
    // If the column doesn't exist, create a new one
    if (!column) {
        column = document.createElement('div');
        column.classList.add(`column-${label}`, 'me-3', 'p-2');
        torrentRow.appendChild(column);
    }
    // Update the content of the existing or newly created column
    const contentParagraph = column.querySelector('p:last-child');
    if (contentParagraph) {
        contentParagraph.textContent = value;
    }
    else {
        column.innerHTML = `<p class="font-weight-bold">${human_label}</p><p>${value}</p>`;
    }
}
// Function to update or create the content of a progress bar column in a torrent row
function updateOrCreateColumnWithProgressBar(torrentRow, label, percentage) {
    let column = torrentRow.querySelector('.column-progress');
    // If the column doesn't exist, create a new one
    if (!column) {
        column = document.createElement('div');
        column.classList.add('column-progress', 'me-3', 'p-2');
        torrentRow.appendChild(column);
    }
    // Update the value of the progress bar in the existing or newly created column
    const progressBar = column.querySelector('.progress-bar');
    const progressPercentage = column.querySelector('p:last-child');
    if (progressBar && progressPercentage) {
        progressBar.style.width = `${percentage}%`;
        progressPercentage.textContent = `${percentage.toFixed(2)}%`;
    }
    else {
        column.innerHTML = `
            <p class="font-weight-bold">${label}</p>
            <div class="progress mb-1">
                <div class="progress-bar" role="progressbar" style="width: ${percentage}%;"></div>
            </div>
            <p class="mb-1">${percentage.toFixed(2)}%</p>`;
    }
}
// Function to render HTML for a torrent row
function renderTorrentRow(torrentsContainer, torrentId, detailsResponse, statsResponse) {
    // Check if the torrent row already exists
    let torrentRow = document.getElementById(`torrent-${torrentId}`);
    // If the torrent row doesn't exist, create a new one
    if (!torrentRow) {
        torrentRow = document.createElement('div');
        torrentRow.id = `torrent-${torrentId}`;
        torrentRow.classList.add('torrent-row', 'd-flex', 'flex-row', 'p-3', 'bg-light', 'rounded', 'mb-3');
        torrentsContainer.appendChild(torrentRow);
    }
    // Update columns in the torrent row
    updateTorrentRow(torrentRow, detailsResponse, statsResponse);
    return torrentRow;
}
// Function to create a column div
function createColumn(label, value, columnClass) {
    const columnDiv = document.createElement('div');
    columnDiv.classList.add(columnClass, 'me-3', 'p-2');
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
