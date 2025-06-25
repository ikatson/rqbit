import { TorrentDetails } from "../api-types";

function getLargestFileName(torrentDetails: TorrentDetails): string | null {
  if (torrentDetails.files.length == 0) {
    return null;
  }
  const largestFile = torrentDetails.files
    .filter((f) => f.included)
    .reduce((prev: any, current: any) =>
      prev.length > current.length ? prev : current
    );
  return largestFile.name;
}

export function torrentDisplayName(
  torrentDetails: TorrentDetails | null
): string {
  if (!torrentDetails) {
    return "";
  }
  return torrentDetails.name ?? getLargestFileName(torrentDetails) ?? "";
}
