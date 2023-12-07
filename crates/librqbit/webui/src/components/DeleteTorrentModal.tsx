import { useContext, useState } from "react";
import { Button, Modal, Form, Spinner } from "react-bootstrap";
import { AppContext, APIContext } from "../context";
import { ErrorWithLabel } from "../rqbit-web";
import { ErrorComponent } from "./ErrorComponent";

export const DeleteTorrentModal: React.FC<{
  id: number;
  show: boolean;
  onHide: () => void;
}> = ({ id, show, onHide }) => {
  if (!show) {
    return null;
  }
  const [deleteFiles, setDeleteFiles] = useState(false);
  const [error, setError] = useState<ErrorWithLabel | null>(null);
  const [deleting, setDeleting] = useState(false);

  const ctx = useContext(AppContext);
  const API = useContext(APIContext);

  const close = () => {
    setDeleteFiles(false);
    setError(null);
    setDeleting(false);
    onHide();
  };

  const deleteTorrent = () => {
    setDeleting(true);

    const call = deleteFiles ? API.delete : API.forget;

    call(id)
      .then(() => {
        ctx.refreshTorrents();
        close();
      })
      .catch((e) => {
        setError({
          text: `Error deleting torrent id=${id}`,
          details: e,
        });
        setDeleting(false);
      });
  };

  return (
    <Modal show={show} onHide={close}>
      <Modal.Header closeButton>Delete torrent</Modal.Header>
      <Modal.Body>
        <Form>
          <Form.Group controlId="delete-torrent">
            <Form.Check
              type="checkbox"
              label="Also delete files"
              checked={deleteFiles}
              onChange={() => setDeleteFiles(!deleteFiles)}
            ></Form.Check>
          </Form.Group>
        </Form>
        {error && <ErrorComponent error={error} />}
      </Modal.Body>
      <Modal.Footer>
        {deleting && <Spinner />}
        <Button variant="primary" onClick={deleteTorrent} disabled={deleting}>
          OK
        </Button>
        <Button variant="secondary" onClick={close}>
          Cancel
        </Button>
      </Modal.Footer>
    </Modal>
  );
};
