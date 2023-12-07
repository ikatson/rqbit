import { TorrentDetails } from "../api-types";

export function getLargestFileName(torrentDetails: TorrentDetails): string {
  const largestFile = torrentDetails.files
    .filter((f) => f.included)
    .reduce((prev: any, current: any) =>
      prev.length > current.length ? prev : current
    );
  return largestFile.name;
}
