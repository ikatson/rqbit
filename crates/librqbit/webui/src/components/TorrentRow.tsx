import { ProgressBar, Row, Spinner } from "react-bootstrap";
import {
  TorrentDetails,
  TorrentStats,
  STATE_INITIALIZING,
  STATE_LIVE,
  STATE_PAUSED,
} from "../api-types";
import { TorrentActions } from "./TorrentActions";
import { Speed } from "./Speed";
import { Column } from "./Column";
import { formatBytes } from "../helper/formatBytes";
import { getLargestFileName } from "../helper/getLargestFileName";
import { getCompletionETA } from "../helper/getCompletionETA";

export const TorrentRow: React.FC<{
  id: number;
  detailsResponse: TorrentDetails | null;
  statsResponse: TorrentStats | null;
}> = ({ id, detailsResponse, statsResponse }) => {
  const state = statsResponse?.state ?? "";
  const error = statsResponse?.error;
  const totalBytes = statsResponse?.total_bytes ?? 1;
  const progressBytes = statsResponse?.progress_bytes ?? 0;
  const finished = statsResponse?.finished || false;
  const progressPercentage = error ? 100 : (progressBytes / totalBytes) * 100;
  const isAnimated =
    (state == STATE_INITIALIZING || state == STATE_LIVE) && !finished;
  const progressLabel = error ? "Error" : `${progressPercentage.toFixed(2)}%`;
  const progressBarVariant = error
    ? "danger"
    : finished
      ? "success"
      : state == STATE_INITIALIZING
        ? "warning"
        : "primary";

  const formatPeersString = () => {
    let peer_stats = statsResponse?.live?.snapshot.peer_stats;
    if (!peer_stats) {
      return "";
    }
    return `${peer_stats.live} / ${peer_stats.seen}`;
  };

  let classNames = [];

  if (error) {
    classNames.push("bg-warning");
  } else {
    if (id % 2 == 0) {
      classNames.push("bg-light");
    }
  }

  return (
    <Row className={classNames.join(" ")}>
      <Column size={3} label="Name">
        {detailsResponse ? (
          <>
            <div className="text-truncate">
              {getLargestFileName(detailsResponse)}
            </div>
            {error && (
              <p className="text-danger">
                <strong>Error:</strong> {error}
              </p>
            )}
          </>
        ) : (
          <Spinner />
        )}
      </Column>
      {statsResponse ? (
        <>
          <Column label="Size">{`${formatBytes(totalBytes)} `}</Column>
          <Column
            size={2}
            label={state == STATE_PAUSED ? "Progress" : "Progress"}
          >
            <ProgressBar
              now={progressPercentage}
              label={progressLabel}
              animated={isAnimated}
              variant={progressBarVariant}
            />
          </Column>
          <Column size={2} label="Speed">
            <Speed statsResponse={statsResponse} />
          </Column>
          <Column label="ETA">{getCompletionETA(statsResponse)}</Column>
          <Column size={2} label="Live / Seen">
            {formatPeersString()}
          </Column>
          <Column label="Actions">
            <TorrentActions id={id} statsResponse={statsResponse} />
          </Column>
        </>
      ) : (
        <Column label="Loading stats" size={8}>
          <Spinner />
        </Column>
      )}
    </Row>
  );
};
