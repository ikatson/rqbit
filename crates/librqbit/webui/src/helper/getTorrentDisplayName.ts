import { TorrentDetails } from "../api-types";

function getLargestFileName(torrentDetails: TorrentDetails): string {
  const largestFile = torrentDetails.files
    .filter((f) => f.included)
    .reduce((prev: any, current: any) =>
      prev.length > current.length ? prev : current
    );
  return largestFile.name;
}

export function torrentDisplayName(torrentDetails: TorrentDetails): string {
  return torrentDetails.name ?? getLargestFileName(torrentDetails);
}
