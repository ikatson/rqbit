import { useContext, useState } from "react";
import { Row, Col } from "react-bootstrap";
import { TorrentStats } from "../api-types";
import { AppContext, APIContext, RefreshTorrentStatsContext } from "../context";
import { IconButton } from "./IconButton";
import { DeleteTorrentModal } from "./DeleteTorrentModal";
import { BsPauseCircle, BsPlayCircle, BsXCircle } from "react-icons/bs";

export const TorrentActions: React.FC<{
  id: number;
  statsResponse: TorrentStats;
}> = ({ id, statsResponse }) => {
  let state = statsResponse.state;

  let [disabled, setDisabled] = useState<boolean>(false);
  let [deleting, setDeleting] = useState<boolean>(false);

  let refreshCtx = useContext(RefreshTorrentStatsContext);

  const canPause = state == "live";
  const canUnpause = state == "paused" || state == "error";

  const ctx = useContext(AppContext);
  const API = useContext(APIContext);

  const unpause = () => {
    setDisabled(true);
    API.start(id)
      .then(
        () => {
          refreshCtx.refresh();
        },
        (e) => {
          ctx.setCloseableError({
            text: `Error starting torrent id=${id}`,
            details: e,
          });
        }
      )
      .finally(() => setDisabled(false));
  };

  const pause = () => {
    setDisabled(true);
    API.pause(id)
      .then(
        () => {
          refreshCtx.refresh();
        },
        (e) => {
          ctx.setCloseableError({
            text: `Error pausing torrent id=${id}`,
            details: e,
          });
        }
      )
      .finally(() => setDisabled(false));
  };

  const startDeleting = () => {
    setDisabled(true);
    setDeleting(true);
  };

  const cancelDeleting = () => {
    setDisabled(false);
    setDeleting(false);
  };

  return (
    <Row>
      <Col>
        {canUnpause && (
          <IconButton onClick={unpause} disabled={disabled} color="success">
            <BsPlayCircle />
          </IconButton>
        )}
        {canPause && (
          <IconButton onClick={pause} disabled={disabled}>
            <BsPauseCircle />
          </IconButton>
        )}
        <IconButton onClick={startDeleting} disabled={disabled} color="danger">
          <BsXCircle />
        </IconButton>
        <DeleteTorrentModal id={id} show={deleting} onHide={cancelDeleting} />
      </Col>
    </Row>
  );
};
