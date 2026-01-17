// Mock API for testing UI with large number of torrents
// This file is only used in dev mode with mock.html entry point

import {
  AddTorrentResponse,
  LimitsConfig,
  ListTorrentsResponse,
  PeerStatsSnapshot,
  RqbitAPI,
  SessionStats,
  TorrentDetails,
  TorrentStats,
  TorrentListItem,
  LiveTorrentStats,
  TorrentFile,
} from "./api-types";

// Torrent name templates for variety
const TORRENT_NAMES = [
  "Ubuntu-{version}-desktop-amd64",
  "Fedora-Workstation-{version}-x86_64",
  "Arch-Linux-{version}",
  "Debian-{version}-amd64-netinst",
  "Linux-Mint-{version}-cinnamon-64bit",
  "PopOS-{version}-amd64-nvidia",
  "Manjaro-{version}-kde-plasma",
  "OpenSUSE-Tumbleweed-{version}",
  "EndeavourOS-{version}-Galileo",
  "Zorin-OS-{version}-Core-64bit",
  "Elementary-OS-{version}-stable",
  "Kali-Linux-{version}-amd64",
  "Parrot-Security-{version}-amd64",
  "NixOS-{version}-x86_64-plasma",
  "Alpine-Linux-{version}-standard",
  "Void-Linux-{version}-x86_64",
  "Gentoo-{version}-amd64-minimal",
  "Slackware-{version}-install-dvd",
  "CentOS-Stream-{version}-x86_64",
  "Rocky-Linux-{version}-minimal",
  "AlmaLinux-{version}-x86_64",
  "FreeBSD-{version}-amd64-dvd",
  "OpenBSD-{version}-amd64",
  "NetBSD-{version}-amd64",
  "SteamOS-{version}-Deck-Recovery",
  "Tails-{version}-amd64-img",
  "Qubes-OS-{version}-x86_64",
  "Whonix-{version}-gateway",
  "MX-Linux-{version}-ahs-x64",
  "AntiX-{version}-full-x64",
  "Solus-{version}-Budgie",
  "Deepin-{version}-amd64",
  "LMDE-{version}-cinnamon-64bit",
  "Garuda-Linux-{version}-dr460nized",
  "ArcoLinux-{version}-plasma",
  "Artix-Linux-{version}-dinit",
  "Calculate-Linux-{version}-desktop",
  "Mageia-{version}-Plasma-x86_64",
  "PCLinuxOS-{version}-kde-darkstar",
  "Puppy-Linux-{version}-bionicpup64",
];

// Very long torrent names (realistic naming patterns with legitimate content)
const LONG_TORRENT_NAMES = [
  "Big.Buck.Bunny.2008.4K.Remastered.2160p.UHD.BluRay.x265.10bit.HDR.DTS-HD.MA.7.1-Blender",
  "Sintel.2010.Open.Movie.Project.Directors.Cut.1080p.BluRay.x264.DTS-HD.MA.5.1-BlenderFoundation",
  "Tears.of.Steel.2012.Creative.Commons.1080p.BluRay.x264.FLAC.5.1-Mango",
  "LibreOffice.Fresh.v24.8.3.Full.Multilingual.x64.Portable-DocumentFoundation",
  "Blender.Studio.Open.Movies.Complete.Collection.2008-2024.4K.UHD.x265.10bit.HDR-BlenderCloud",
  "Cosmos.Laundromat.2015.First.Cycle.Open.Movie.2160p.UHD.HDR.x265.DTS-HD.MA.7.1.Atmos-Gooseberry",
  "Spring.2019.Open.Movie.Blender.Animation.Studio.2160p.UHD.BluRay.x265.10bit.HDR.FLAC.5.1-BlenderStudio",
  "Agent.327.Operation.Barbershop.2017.Blender.Institute.1080p.BluRay.x264.DTS-HD.MA.5.1-BlenderFoundation",
  "Wikipedia.Offline.Complete.English.2024.Compressed.Archive.Split.Parts.001-100-Kiwix",
  "Internet.Archive.Public.Domain.Movies.Collection.Vol.42.1080p.Restored.x264-ArchiveOrg",
];

// File name templates
const FILE_EXTENSIONS = [".iso", ".img", ".tar.gz", ".zip", ".qcow2"];

// Generate deterministic random number from seed
function seededRandom(seed: number): () => number {
  return () => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed / 0x7fffffff;
  };
}

// Generate a fake info_hash from id
function generateInfoHash(id: number): string {
  const chars = "0123456789abcdef";
  const rand = seededRandom(id * 31337);
  let hash = "";
  for (let i = 0; i < 40; i++) {
    hash += chars[Math.floor(rand() * 16)];
  }
  return hash;
}

// Generate torrent name from id
function generateTorrentName(id: number): string {
  const rand = seededRandom(id);
  // Every ~10th torrent gets a long name
  if (rand() < 0.1) {
    return LONG_TORRENT_NAMES[Math.floor(rand() * LONG_TORRENT_NAMES.length)];
  }
  const template = TORRENT_NAMES[Math.floor(rand() * TORRENT_NAMES.length)];
  const version = `${Math.floor(rand() * 30) + 1}.${Math.floor(rand() * 12)}.${Math.floor(rand() * 20)}`;
  return template.replace("{version}", version);
}

// State weights for distribution
type TorrentState = "live" | "paused" | "initializing" | "error";

// Limit concurrent active torrents to be more realistic
const MAX_CONCURRENT_ACTIVE = 30;

function generateState(id: number): TorrentState {
  // Only first MAX_CONCURRENT_ACTIVE torrents are active by default
  // The rest are paused with some errors mixed in
  if (id < MAX_CONCURRENT_ACTIVE) {
    const rand = seededRandom(id * 7919);
    const r = rand();
    if (r < 0.85) return "live";
    if (r < 0.95) return "initializing";
    return "error";
  } else {
    // Most are paused, few have errors
    const rand = seededRandom(id * 7919);
    const r = rand();
    if (r < 0.95) return "paused";
    return "error";
  }
}

// Generate file size in bytes (500MB to 10GB)
function generateTotalBytes(id: number): number {
  const rand = seededRandom(id * 1013);
  const minSize = 500 * 1024 * 1024; // 500MB
  const maxSize = 10 * 1024 * 1024 * 1024; // 10GB
  return Math.floor(rand() * (maxSize - minSize) + minSize);
}

// Track progress over time for live torrents
const progressTracker = new Map<number, number>();

function getProgressBytes(
  id: number,
  totalBytes: number,
  state: TorrentState,
): number {
  if (state === "initializing") return 0;

  let progress = progressTracker.get(id);
  if (progress === undefined) {
    // Initialize with random progress
    const rand = seededRandom(id * 2749);
    progress = rand() * totalBytes;
    progressTracker.set(id, progress);
  }

  // Simulate progress for live torrents
  if (state === "live" && progress < totalBytes) {
    const increment = Math.random() * 5 * 1024 * 1024; // Up to 5MB per poll
    progress = Math.min(progress + increment, totalBytes);
    progressTracker.set(id, progress);
  }

  return Math.floor(progress);
}

// Generate files for a torrent
function generateFiles(id: number, totalBytes: number): TorrentFile[] {
  const rand = seededRandom(id * 4231);
  const name = generateTorrentName(id);
  const numFiles = Math.floor(rand() * 5) + 1; // 1-5 files

  const files: TorrentFile[] = [];
  let remainingBytes = totalBytes;

  for (let i = 0; i < numFiles; i++) {
    const isLast = i === numFiles - 1;
    const fileSize = isLast
      ? remainingBytes
      : Math.floor(rand() * remainingBytes * 0.7);
    remainingBytes -= fileSize;

    const ext = FILE_EXTENSIONS[Math.floor(rand() * FILE_EXTENSIONS.length)];
    const fileName =
      numFiles === 1 ? `${name}${ext}` : `${name}.part${i + 1}${ext}`;

    files.push({
      name: fileName,
      components: [fileName],
      length: fileSize,
      included: rand() > 0.1, // 90% included
      attributes: {
        symlink: false,
        hidden: false,
        padding: false,
        executable: false,
      },
    });
  }

  return files;
}

// Generate live stats
function generateLiveStats(
  id: number,
  progressBytes: number,
  totalBytes: number,
): LiveTorrentStats {
  const rand = seededRandom(id * 8737 + (Date.now() % 10000));
  const remainingBytes = totalBytes - progressBytes;
  const downloadSpeed = Math.random() * 50; // 0-50 Mbps
  const uploadSpeed = Math.random() * 10; // 0-10 Mbps

  const downloadBytesPerSec = (downloadSpeed * 1024 * 1024) / 8;
  const etaSecs =
    downloadBytesPerSec > 0 ? remainingBytes / downloadBytesPerSec : null;

  return {
    snapshot: {
      have_bytes: progressBytes,
      downloaded_and_checked_bytes: progressBytes,
      downloaded_and_checked_pieces: Math.floor(
        (progressBytes / totalBytes) * 1000,
      ),
      fetched_bytes: progressBytes,
      uploaded_bytes: Math.floor(rand() * progressBytes * 0.5),
      initially_needed_bytes: totalBytes,
      remaining_bytes: remainingBytes,
      total_bytes: totalBytes,
      total_piece_download_ms: Math.floor(rand() * 100000),
      peer_stats: {
        queued: Math.floor(rand() * 50),
        connecting: Math.floor(rand() * 10),
        live: Math.floor(rand() * 30) + 1,
        seen: Math.floor(rand() * 200),
        dead: Math.floor(rand() * 100),
        not_needed: Math.floor(rand() * 20),
      },
    },
    average_piece_download_time: {
      secs: Math.floor(rand() * 2),
      nanos: Math.floor(rand() * 1000000000),
    },
    download_speed: {
      mbps: downloadSpeed,
      human_readable: `${downloadSpeed.toFixed(1)} MB/s`,
    },
    upload_speed: {
      mbps: uploadSpeed,
      human_readable: `${uploadSpeed.toFixed(1)} MB/s`,
    },
    all_time_download_speed: {
      mbps: downloadSpeed * 0.8,
      human_readable: `${(downloadSpeed * 0.8).toFixed(1)} MB/s`,
    },
    time_remaining:
      etaSecs !== null
        ? {
            human_readable:
              etaSecs < 60
                ? `${Math.floor(etaSecs)}s`
                : etaSecs < 3600
                  ? `${Math.floor(etaSecs / 60)}m`
                  : `${Math.floor(etaSecs / 3600)}h ${Math.floor((etaSecs % 3600) / 60)}m`,
            duration: { secs: Math.floor(etaSecs) },
          }
        : null,
  };
}

// Generate torrent stats
function generateTorrentStats(id: number): TorrentStats {
  const state = generateState(id);
  const totalBytes = generateTotalBytes(id);
  const progressBytes = getProgressBytes(id, totalBytes, state);
  const finished = progressBytes >= totalBytes;

  const rand = seededRandom(id * 5501);
  const numFiles = Math.floor(rand() * 5) + 1;
  const fileProgress = Array(numFiles)
    .fill(0)
    .map(() => (finished ? 1 : rand() * (progressBytes / totalBytes)));

  return {
    state: finished && state === "live" ? "live" : state,
    error: state === "error" ? "Connection timed out" : null,
    file_progress: fileProgress,
    progress_bytes: progressBytes,
    finished,
    total_bytes: totalBytes,
    live:
      state === "live"
        ? generateLiveStats(id, progressBytes, totalBytes)
        : null,
  };
}

// Generate torrent list item
function generateTorrentListItem(
  id: number,
  withStats: boolean,
): TorrentListItem {
  const totalBytes = generateTotalBytes(id);
  const totalPieces = Math.ceil(totalBytes / (256 * 1024)); // 256KB pieces

  const item: TorrentListItem = {
    id,
    info_hash: generateInfoHash(id),
    name: generateTorrentName(id),
    output_folder: `/downloads/torrent_${id}`,
    total_pieces: totalPieces,
  };

  if (withStats) {
    item.stats = generateTorrentStats(id);
  }

  return item;
}

// Store for tracking torrent state changes
const torrentStates = new Map<number, TorrentState>();
const deletedTorrents = new Set<number>();

// Store stable peer data per torrent
interface PeerData {
  ip: string;
  port: number;
  connKind: "tcp" | "utp";
  // Counters that grow over time
  fetchedBytes: number;
  uploadedBytes: number;
  fetchRate: number; // bytes per second baseline
  uploadRate: number;
}

const torrentPeers = new Map<number, PeerData[]>();
const peerLastUpdate = new Map<number, number>();

function getOrCreatePeers(torrentId: number): PeerData[] {
  let peers = torrentPeers.get(torrentId);
  if (!peers) {
    const rand = seededRandom(torrentId * 9371);
    const numPeers = Math.floor(rand() * 15) + 5; // 5-20 peers
    peers = [];

    for (let i = 0; i < numPeers; i++) {
      peers.push({
        ip: `${Math.floor(rand() * 256)}.${Math.floor(rand() * 256)}.${Math.floor(rand() * 256)}.${Math.floor(rand() * 256)}`,
        port: 6881 + Math.floor(rand() * 1000),
        connKind: rand() > 0.3 ? "tcp" : "utp",
        fetchedBytes: Math.floor(rand() * 10000000), // Initial bytes
        uploadedBytes: Math.floor(rand() * 5000000),
        fetchRate: Math.floor(rand() * 2000000) + 100000, // 100KB-2MB/s
        uploadRate: Math.floor(rand() * 500000) + 50000, // 50KB-500KB/s
      });
    }
    torrentPeers.set(torrentId, peers);
    peerLastUpdate.set(torrentId, Date.now());
  }
  return peers;
}

function updatePeerCounters(torrentId: number): void {
  const peers = torrentPeers.get(torrentId);
  const lastUpdate = peerLastUpdate.get(torrentId);
  if (!peers || !lastUpdate) return;

  const now = Date.now();
  const elapsed = (now - lastUpdate) / 1000; // seconds
  peerLastUpdate.set(torrentId, now);

  // Only update if torrent is live
  const state = torrentStates.get(torrentId) ?? generateState(torrentId);
  if (state !== "live") return;

  for (const peer of peers) {
    // Add some variance to the rates
    const fetchVariance = 0.5 + Math.random();
    const uploadVariance = 0.5 + Math.random();
    peer.fetchedBytes += Math.floor(peer.fetchRate * elapsed * fetchVariance);
    peer.uploadedBytes += Math.floor(
      peer.uploadRate * elapsed * uploadVariance,
    );
  }
}

const TOTAL_TORRENTS = 1000;

// Mock API implementation
export const MockAPI: RqbitAPI & { getVersion: () => Promise<string> } = {
  getStreamLogsUrl: () => null,

  listTorrents: async (opts?: {
    withStats?: boolean;
  }): Promise<ListTorrentsResponse> => {
    // Simulate network delay
    await new Promise((r) => setTimeout(r, 50 + Math.random() * 100));

    const torrents: TorrentListItem[] = [];
    for (let id = 0; id < TOTAL_TORRENTS; id++) {
      if (deletedTorrents.has(id)) continue;
      torrents.push(generateTorrentListItem(id, opts?.withStats ?? false));
    }

    return { torrents };
  },

  getTorrentDetails: async (index: number): Promise<TorrentDetails> => {
    await new Promise((r) => setTimeout(r, 20 + Math.random() * 50));

    if (deletedTorrents.has(index)) {
      throw { text: "Torrent not found", status: 404 };
    }

    const totalBytes = generateTotalBytes(index);
    return {
      name: generateTorrentName(index),
      info_hash: generateInfoHash(index),
      files: generateFiles(index, totalBytes),
      total_pieces: Math.ceil(totalBytes / (256 * 1024)),
      output_folder: `/downloads/torrent_${index}`,
    };
  },

  getTorrentStats: async (index: number): Promise<TorrentStats> => {
    await new Promise((r) => setTimeout(r, 10 + Math.random() * 30));

    if (deletedTorrents.has(index)) {
      throw { text: "Torrent not found", status: 404 };
    }

    // Check for manual state override
    const override = torrentStates.get(index);
    const stats = generateTorrentStats(index);

    if (override) {
      stats.state = override;
      if (override !== "live") {
        stats.live = null;
      }
    }

    return stats;
  },

  getPeerStats: async (index: number): Promise<PeerStatsSnapshot> => {
    await new Promise((r) => setTimeout(r, 20));

    // Get stable peers and update their counters
    const peerList = getOrCreatePeers(index);
    updatePeerCounters(index);

    const peers: Record<string, any> = {};
    const rand = seededRandom(index * 4421); // For other random values

    for (const peer of peerList) {
      peers[`${peer.ip}:${peer.port}`] = {
        counters: {
          incoming_connections: Math.floor(rand() * 10),
          fetched_bytes: peer.fetchedBytes,
          uploaded_bytes: peer.uploadedBytes,
          total_time_connecting_ms: Math.floor(rand() * 10000) + 1000,
          connection_attempts: Math.floor(rand() * 3) + 1,
          connections: 1,
          errors: Math.floor(rand() * 2),
          fetched_chunks: Math.floor(peer.fetchedBytes / 16384), // ~16KB chunks
          downloaded_and_checked_pieces: Math.floor(peer.fetchedBytes / 262144), // ~256KB pieces
          total_piece_download_ms: Math.floor(rand() * 50000) + 5000,
          times_stolen_from_me: 0,
          times_i_stole: 0,
        },
        state: "live",
        conn_kind: peer.connKind,
      };
    }

    return { peers };
  },

  stats: async (): Promise<SessionStats> => {
    await new Promise((r) => setTimeout(r, 30));

    const downloadSpeed = Math.random() * 100;
    const uploadSpeed = Math.random() * 30;

    return {
      counters: {
        fetched_bytes: Math.floor(Math.random() * 100000000000),
        uploaded_bytes: Math.floor(Math.random() * 50000000000),
        blocked_incoming: Math.floor(Math.random() * 100),
        blocked_outgoing: Math.floor(Math.random() * 50),
      },
      peers: {
        queued: Math.floor(Math.random() * 500),
        connecting: Math.floor(Math.random() * 100),
        live: Math.floor(Math.random() * 300) + 50,
        seen: Math.floor(Math.random() * 2000),
        dead: Math.floor(Math.random() * 500),
        not_needed: Math.floor(Math.random() * 200),
      },
      connections: {
        tcp: {
          v4: { attempts: 1000, successes: 800, errors: 200 },
          v6: { attempts: 200, successes: 150, errors: 50 },
        },
        utp: {
          v4: { attempts: 500, successes: 300, errors: 200 },
          v6: { attempts: 100, successes: 60, errors: 40 },
        },
        socks: {
          v4: { attempts: 0, successes: 0, errors: 0 },
          v6: { attempts: 0, successes: 0, errors: 0 },
        },
      },
      download_speed: {
        mbps: downloadSpeed,
        human_readable: `${downloadSpeed.toFixed(1)} MB/s`,
      },
      upload_speed: {
        mbps: uploadSpeed,
        human_readable: `${uploadSpeed.toFixed(1)} MB/s`,
      },
      uptime_seconds: Math.floor(Date.now() / 1000) % 86400,
    };
  },

  uploadTorrent: async (): Promise<AddTorrentResponse> => {
    throw { text: "Upload not supported in mock mode", status: 501 };
  },

  updateOnlyFiles: async (): Promise<void> => {
    await new Promise((r) => setTimeout(r, 100));
  },

  pause: async (index: number): Promise<void> => {
    await new Promise((r) => setTimeout(r, 50));
    torrentStates.set(index, "paused");
  },

  start: async (index: number): Promise<void> => {
    await new Promise((r) => setTimeout(r, 50));
    torrentStates.set(index, "live");
  },

  forget: async (index: number): Promise<void> => {
    await new Promise((r) => setTimeout(r, 50));
    deletedTorrents.add(index);
  },

  delete: async (index: number): Promise<void> => {
    await new Promise((r) => setTimeout(r, 100));
    deletedTorrents.add(index);
  },

  getVersion: async (): Promise<string> => {
    return "mock-1.0.0";
  },

  getTorrentStreamUrl: () => null,
  getPlaylistUrl: () => null,

  getTorrentHaves: async (index: number): Promise<Uint8Array> => {
    const totalBytes = generateTotalBytes(index);
    const totalPieces = Math.ceil(totalBytes / (256 * 1024));
    const bytes = Math.ceil(totalPieces / 8);
    const haves = new Uint8Array(bytes);

    const rand = seededRandom(index * 6173);
    const progress =
      getProgressBytes(index, totalBytes, generateState(index)) / totalBytes;

    for (let i = 0; i < bytes; i++) {
      let byte = 0;
      for (let bit = 0; bit < 8; bit++) {
        if (rand() < progress) {
          byte |= 1 << (7 - bit);
        }
      }
      haves[i] = byte;
    }

    return haves;
  },

  getLimits: async (): Promise<LimitsConfig> => {
    return { upload_bps: null, download_bps: null };
  },

  setLimits: async (): Promise<void> => {
    await new Promise((r) => setTimeout(r, 50));
  },
};
