import { useContext, useState } from "react";
import { TorrentListItem } from "../../api-types";
import { APIContext } from "../../context";
import { ErrorWithLabel } from "../../rqbit-web";
import { useTorrentStore } from "../../stores/torrentStore";
import { useUIStore } from "../../stores/uiStore";
import { Button } from "../buttons/Button";
import { ErrorComponent } from "../ErrorComponent";
import { Spinner } from "../Spinner";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";

export const DeleteTorrentModal: React.FC<{
  show: boolean;
  onHide: () => void;
  torrents: Pick<TorrentListItem, "id" | "name">[];
}> = ({ show, onHide, torrents }) => {
  const [deleteFiles, setDeleteFiles] = useState(false);
  const [error, setError] = useState<ErrorWithLabel | null>(null);
  const [deleting, setDeleting] = useState(false);

  const API = useContext(APIContext);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const clearSelection = useUIStore((state) => state.clearSelection);

  if (!show || torrents.length === 0) {
    return null;
  }

  const close = () => {
    setDeleteFiles(false);
    setError(null);
    setDeleting(false);
    onHide();
  };

  const deleteTorrents = async () => {
    setDeleting(true);
    setError(null);

    const deleteMethod = deleteFiles ? API.delete : API.forget;
    const errors: string[] = [];

    for (const torrent of torrents) {
      try {
        await deleteMethod(torrent.id);
      } catch (e) {
        const name = torrent.name || `id=${torrent.id}`;
        errors.push(`${name}: ${e}`);
      }
    }

    if (errors.length > 0) {
      setError({
        text: `Failed to delete ${errors.length} torrent${errors.length > 1 ? "s" : ""}`,
        details: { text: errors.join("\n") },
      });
      setDeleting(false);
    } else {
      clearSelection();
      refreshTorrents();
      close();
    }
  };

  const isBulk = torrents.length > 1;
  const title = isBulk
    ? `Delete ${torrents.length} torrents`
    : "Delete torrent";

  return (
    <Modal isOpen={show} onClose={onHide} title={title}>
      <ModalBody>
        <p className="text-gray-700 dark:text-slate-300 mb-3">
          {isBulk
            ? "Are you sure you want to delete the following torrents?"
            : "Are you sure you want to delete this torrent?"}
        </p>

        <div
          className={`rounded-md bg-gray-50 dark:bg-slate-700/50 p-3 ${
            isBulk ? "max-h-48 overflow-y-auto" : ""
          }`}
        >
          <ul className="space-y-1">
            {torrents.map((torrent) => (
              <li
                key={torrent.id}
                className="text-gray-800 dark:text-slate-200 truncate"
                title={torrent.name ?? undefined}
              >
                <span className="font-medium">
                  {torrent.name || `Torrent #${torrent.id}`}
                </span>
              </li>
            ))}
          </ul>
        </div>

        <div className="mt-4 flex items-center">
          <input
            type="checkbox"
            id="deleteFiles"
            className="form-checkbox h-4 w-4 text-blue-500 rounded border-gray-300 dark:border-slate-600"
            onChange={() => setDeleteFiles(!deleteFiles)}
            checked={deleteFiles}
          />
          <label
            htmlFor="deleteFiles"
            className="ml-2 text-gray-700 dark:text-slate-300"
          >
            Also delete downloaded files
          </label>
        </div>

        {error && <ErrorComponent error={error} />}
      </ModalBody>

      <ModalFooter>
        {deleting && <Spinner />}
        <Button variant="cancel" onClick={close}>
          Cancel
        </Button>
        <Button variant="danger" onClick={deleteTorrents} disabled={deleting}>
          {isBulk ? `Delete ${torrents.length} Torrents` : "Delete Torrent"}
        </Button>
      </ModalFooter>
    </Modal>
  );
};
