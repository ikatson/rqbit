import { useContext, useEffect, useState } from "react";
import { Button, Form } from "react-bootstrap";
import { AddTorrentResponse, AddTorrentOptions } from "../../api-types";
import { AppContext, APIContext } from "../../context";
import { ErrorComponent } from "../ErrorComponent";
import { formatBytes } from "../../helper/formatBytes";
import { ErrorWithLabel } from "../../rqbit-web";
import { Spinner } from "../Spinner";
import { Modal } from "./Modal";
// import useModal from "../useModal";

export const FileSelectionModal = (props: {
  onHide: () => void;
  listTorrentResponse: AddTorrentResponse | null;
  listTorrentError: ErrorWithLabel | null;
  listTorrentLoading: boolean;
  data: string | File;
}) => {
  let {
    onHide,
    listTorrentResponse,
    listTorrentError,
    listTorrentLoading,
    data,
  } = props;

  const [selectedFiles, setSelectedFiles] = useState<number[]>([]);
  const [uploading, setUploading] = useState(false);
  const [uploadError, setUploadError] = useState<ErrorWithLabel | null>(null);
  const [unpopularTorrent, setUnpopularTorrent] = useState(false);
  const [outputFolder, setOutputFolder] = useState<string>("");
  const ctx = useContext(AppContext);
  const API = useContext(APIContext);
  // const [Modal, , , closeModal] = useModal({ fullScreen: true });

  useEffect(() => {
    console.log(listTorrentResponse);
    setSelectedFiles(
      listTorrentResponse
        ? listTorrentResponse.details.files.map((_, id) => id)
        : []
    );
    setOutputFolder(listTorrentResponse?.output_folder || "");
  }, [listTorrentResponse]);

  const clear = () => {
    onHide();
    setSelectedFiles([]);
    setUploadError(null);
    setUploading(false);
  };

  const handleToggleFile = (toggledId: number) => {
    if (selectedFiles.includes(toggledId)) {
      setSelectedFiles(selectedFiles.filter((i) => i !== toggledId));
    } else {
      setSelectedFiles([...selectedFiles, toggledId]);
    }
  };

  const handleUpload = async () => {
    if (!listTorrentResponse) {
      return;
    }
    setUploading(true);
    let initialPeers = listTorrentResponse.seen_peers
      ? listTorrentResponse.seen_peers.slice(0, 32)
      : null;
    let opts: AddTorrentOptions = {
      overwrite: true,
      only_files: selectedFiles,
      initial_peers: initialPeers,
      output_folder: outputFolder,
    };
    if (unpopularTorrent) {
      opts.peer_opts = {
        connect_timeout: 20,
        read_write_timeout: 60,
      };
    }
    API.uploadTorrent(data, opts)
      .then(
        () => {
          onHide();
          ctx.refreshTorrents();
        },
        (e) => {
          setUploadError({ text: "Error starting torrent", details: e });
        }
      )
      .finally(() => setUploading(false));
  };

  const getBody = () => {
    if (listTorrentLoading) {
      return <Spinner />;
    } else if (listTorrentError) {
      return <ErrorComponent error={listTorrentError}></ErrorComponent>;
    } else if (listTorrentResponse) {
      return (
        <Form>
          <fieldset className="mb-4">
            <legend>Pick the files to download</legend>
            {listTorrentResponse.details.files.map((file, index) => (
              <Form.Group key={index} controlId={`check-${index}`}>
                <Form.Check
                  type="checkbox"
                  label={`${file.name}  (${formatBytes(file.length)})`}
                  checked={selectedFiles.includes(index)}
                  onChange={() => handleToggleFile(index)}
                ></Form.Check>
              </Form.Group>
            ))}
          </fieldset>
          <fieldset>
            <legend>Options</legend>
            <Form.Group controlId="output-folder" className="mb-3">
              <Form.Label>Output folder</Form.Label>
              <Form.Control
                type="text"
                value={outputFolder}
                onChange={(e) => setOutputFolder(e.target.value)}
              />
            </Form.Group>
            <Form.Group controlId="unpopular-torrent" className="mb-3">
              <Form.Check
                type="checkbox"
                label="Increase timeouts"
                checked={unpopularTorrent}
                onChange={() => setUnpopularTorrent(!unpopularTorrent)}
              ></Form.Check>
              <small id="emailHelp" className="form-text text-muted">
                This might be useful for unpopular torrents with few peers. It
                will slow down fast torrents though.
              </small>
            </Form.Group>
          </fieldset>
        </Form>
      );
    }
  };
  return (
    <Modal isOpen={true} onClose={clear} title="Add Torrent">
      {getBody()}
      <ErrorComponent error={uploadError} />
      <div id="footer" className="flex justify-end gap-4">
        {uploading && <Spinner />}
        <button onClick={clear}>Cancel</button>
        <button
          onClick={handleUpload}
          disabled={
            listTorrentLoading || uploading || selectedFiles.length == 0
          }
        >
          OK
        </button>
      </div>
    </Modal>
  );
  // return (
  //   <Modal show onHide={clear} size="lg">
  //     <Modal.Header closeButton>
  //       <Modal.Title>Add torrent</Modal.Title>
  //     </Modal.Header>
  //     <Modal.Body>
  //       {getBody()}
  //       <ErrorComponent error={uploadError} />
  //     </Modal.Body>
  //     <Modal.Footer>
  //       {uploading && <Spinner />}
  //       <Button
  //         variant="primary"
  //         onClick={handleUpload}
  //         disabled={
  //           listTorrentLoading || uploading || selectedFiles.length == 0
  //         }
  //       >
  //         OK
  //       </Button>
  //       <Button variant="secondary" onClick={clear}>
  //         Cancel
  //       </Button>
  //     </Modal.Footer>
  //   </Modal>
  // );
};
