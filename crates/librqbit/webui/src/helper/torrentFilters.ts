// Shared torrent filtering and sorting utilities

import { TorrentListItem } from "../api-types";

// Sort types
export type TorrentSortColumn =
  | "id"
  | "name"
  | "size"
  | "progress"
  | "downSpeed"
  | "upSpeed"
  | "eta";

export type SortDirection = "asc" | "desc";

// Status filter types
export type StatusFilter =
  | "all"
  | "downloading"
  | "seeding"
  | "paused"
  | "error";

// Sort column display labels
export const SORT_COLUMN_LABELS: Record<TorrentSortColumn, string> = {
  id: "ID",
  name: "Name",
  size: "Size",
  progress: "Progress",
  downSpeed: "Down Speed",
  upSpeed: "Up Speed",
  eta: "ETA",
};

// Status filter display labels
export const STATUS_FILTER_LABELS: Record<StatusFilter, string> = {
  all: "All",
  downloading: "Downloading",
  seeding: "Seeding",
  paused: "Paused",
  error: "Error",
};

// Get sort value for a torrent
export function getSortValue(
  t: TorrentListItem,
  column: TorrentSortColumn,
): number | string {
  switch (column) {
    case "id":
      return t.id;
    case "name":
      return (t.name ?? "").toLowerCase();
    case "size":
      return t.stats?.total_bytes ?? 0;
    case "progress":
      return t.stats?.total_bytes
        ? (t.stats.progress_bytes ?? 0) / t.stats.total_bytes
        : 0;
    case "downSpeed":
      return t.stats?.live?.download_speed?.mbps ?? 0;
    case "upSpeed":
      return t.stats?.live?.upload_speed?.mbps ?? 0;
    case "eta": {
      if (!t.stats?.live) return Infinity;
      const remaining =
        (t.stats.total_bytes ?? 0) - (t.stats.progress_bytes ?? 0);
      const speed = t.stats.live.download_speed?.mbps ?? 0;
      if (speed <= 0 || remaining <= 0) return remaining <= 0 ? 0 : Infinity;
      return remaining / (speed * 1024 * 1024);
    }
  }
}

// Compare function for sorting
export function compareTorrents(
  a: TorrentListItem,
  b: TorrentListItem,
  column: TorrentSortColumn,
  direction: SortDirection,
): number {
  const aVal = getSortValue(a, column);
  const bVal = getSortValue(b, column);
  const cmp =
    typeof aVal === "string"
      ? aVal.localeCompare(bVal as string)
      : (aVal as number) - (bVal as number);
  return direction === "asc" ? cmp : -cmp;
}

// Check if torrent matches search query
export function matchesSearch(name: string | null, query: string): boolean {
  if (!query) return true;
  return (name ?? "").toLowerCase().includes(query);
}

// Check if torrent matches status filter
export function matchesStatus(
  t: TorrentListItem,
  filter: StatusFilter,
): boolean {
  if (filter === "all") return true;

  const state = t.stats?.state;
  const finished = t.stats?.finished;

  switch (filter) {
    case "downloading":
      return state === "live" && !finished;
    case "seeding":
      return state === "live" && !!finished;
    case "paused":
      return state === "paused";
    case "error":
      return state === "error";
  }
}

// Combined visibility check
export function isTorrentVisible(
  t: TorrentListItem,
  searchQuery: string,
  statusFilter: StatusFilter,
): boolean {
  return matchesSearch(t.name, searchQuery) && matchesStatus(t, statusFilter);
}
