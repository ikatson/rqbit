import { useContext, useState } from "react";
import { AppContext, APIContext } from "../../context";
import { ErrorWithLabel } from "../../rqbit-web";
import { ErrorComponent } from "../ErrorComponent";
import useModal from "../useModal";
import { Spinner } from "../Spinner";

export const DeleteTorrentModal: React.FC<{
  id: number;
  show: boolean;
  onHide: () => void;
}> = ({ id, show, onHide }) => {
  if (!show) {
    return null;
  }
  const [Modal, isOpen, , closeModal] = useModal();

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
    <Modal
      isOpen={show || isOpen}
      closeModal={() => {
        closeModal();
        onHide();
      }}
    >
      <h1 className="text-xl mb-2">Delete torrent</h1>
      <div className="flex my-3 justify-start gap-4">
        <div className="mt-2">
          <p className="text-gray-700">
            Are you sure you want to delete the torrent?
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
            <label htmlFor="deleteFiles" className="ml-2 text-gray-700">
              Also delete files
            </label>
          </div>
        </div>
      </div>
      {error && <ErrorComponent error={error} />}
      <div className="flex gap-2 justify-end">
        {deleting && <Spinner />}
        <button
          className="p-2 rounded-lg border-1 border-red-50 drop-shadow-md hover:border-slate-400 hover:text-slate-500"
          onClick={close}
        >
          Cancel
        </button>
        <button
          className="p-2 border-1 bg-red-500 text-white px-4 py-2 rounded border-green-50  hover:border-red-700 hover:bg-red-600"
          onClick={deleteTorrent}
          disabled={deleting}
        >
          Delete Torrent
        </button>
      </div>
    </Modal>
  );
};
