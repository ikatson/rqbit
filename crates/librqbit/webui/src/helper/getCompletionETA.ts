import { TorrentStats } from "../api-types";
import { formatSecondsToTime } from "./formatSecondsToTime";

export function getCompletionETA(stats: TorrentStats): string {
  let duration = stats?.live?.time_remaining?.duration?.secs;
  if (duration == null) {
    return "N/A";
  }
  return formatSecondsToTime(duration);
}
