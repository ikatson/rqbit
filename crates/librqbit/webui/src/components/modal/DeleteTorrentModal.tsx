import { useContext, useState } from "react";
import { TorrentDetails } from "../../api-types";
import { APIContext } from "../../context";
import { torrentDisplayName } from "../../helper/getTorrentDisplayName";
import { ErrorWithLabel } from "../../rqbit-web";
import { useTorrentStore } from "../../stores/torrentStore";
import { Button } from "../buttons/Button";
import { ErrorComponent } from "../ErrorComponent";
import { Spinner } from "../Spinner";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";

export const DeleteTorrentModal: React.FC<{
  id: number;
  show: boolean;
  onHide: () => void;
  torrent: TorrentDetails | null;
}> = ({ id, show, onHide, torrent }) => {
  if (!show) {
    return null;
  }
  const [deleteFiles, setDeleteFiles] = useState(false);
  const [error, setError] = useState<ErrorWithLabel | null>(null);
  const [deleting, setDeleting] = useState(false);

  const API = useContext(APIContext);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

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
        refreshTorrents();
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
    <Modal isOpen={show} onClose={onHide} title="Delete torrent">
      <ModalBody>
        <p></p>
        <p className="text-gray-700 dark:text-slate-300">
          Are you sure you want to delete{" "}
          <span className="font-medium">"{torrentDisplayName(torrent)}"</span>?
        </p>

        <div className="mt-4 flex items-center">
          <input
            type="checkbox"
            id="deleteFiles"
            className="form-checkbox h-4 w-4 text-blue-500"
            onChange={() => setDeleteFiles(!deleteFiles)}
            checked={deleteFiles}
            placeholder="Also delete files"
          />
          <label
            htmlFor="deleteFiles"
            className="ml-2 text-gray-700 dark:text-slate-300"
          >
            Also delete files
          </label>
        </div>

        {error && <ErrorComponent error={error} />}
      </ModalBody>

      <ModalFooter>
        {deleting && <Spinner />}
        <Button variant="cancel" onClick={close}>
          Cancel
        </Button>
        <Button variant="danger" onClick={deleteTorrent} disabled={deleting}>
          Delete Torrent
        </Button>
      </ModalFooter>
    </Modal>
  );
};
