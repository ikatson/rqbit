import { useContext, useEffect, useState } from "react";
import { AddTorrentResponse, AddTorrentOptions } from "../../api-types";
import { APIContext } from "../../context";
import { ErrorComponent } from "../ErrorComponent";
import { ErrorWithLabel } from "../../rqbit-web";
import { Spinner } from "../Spinner";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";
import { Button } from "../buttons/Button";
import { Fieldset } from "../forms/Fieldset";
import { FormInput } from "../forms/FormInput";
import { Form } from "../forms/Form";
import { FileListInput } from "../FileListInput";
import { useTorrentStore } from "../../stores/torrentStore";

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

  const [selectedFiles, setSelectedFiles] = useState<Set<number>>(new Set());
  const [uploading, setUploading] = useState(false);
  const [uploadError, setUploadError] = useState<ErrorWithLabel | null>(null);
  const [unpopularTorrent, setUnpopularTorrent] = useState(false);
  const [outputFolder, setOutputFolder] = useState<string>("");
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const API = useContext(APIContext);

  useEffect(() => {
    setSelectedFiles(
      new Set(listTorrentResponse?.details.files.map((_, i) => i))
    );
    setOutputFolder(listTorrentResponse?.output_folder || "");
  }, [listTorrentResponse]);

  const clear = () => {
    onHide();
    setSelectedFiles(new Set());
    setUploadError(null);
    setUploading(false);
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
      only_files: Array.from(selectedFiles),
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
          refreshTorrents();
        },
        (e) => {
          setUploadError({ text: "Error starting torrent", details: e });
        }
      )
      .finally(() => setUploading(false));
  };

  const getBody = () => {
    if (listTorrentLoading) {
      return <Spinner label="Loading torrent contents" />;
    } else if (listTorrentError) {
      return <ErrorComponent error={listTorrentError}></ErrorComponent>;
    } else if (listTorrentResponse) {
      return (
        <Form>
          <FormInput
            label="Output folder"
            name="output_folder"
            inputType="text"
            value={outputFolder}
            onChange={(e) => setOutputFolder(e.target.value)}
          />

          <Fieldset>
            <FileListInput
              selectedFiles={selectedFiles}
              setSelectedFiles={setSelectedFiles}
              listTorrentResponse={listTorrentResponse}
            />
          </Fieldset>

          {/* <Fieldset label="Options">
            <FormCheckbox
              label="Increase timeouts"
              checked={unpopularTorrent}
              onChange={() => setUnpopularTorrent(!unpopularTorrent)}
              help="This might be useful for unpopular torrents with few peers. It will slow down fast torrents though."
              name="increase_timeouts"
            />
          </Fieldset> */}
        </Form>
      );
    }
  };
  return (
    <Modal isOpen={true} onClose={clear} title="Add Torrent">
      <ModalBody>
        {getBody()}
        <ErrorComponent error={uploadError} />
      </ModalBody>
      <ModalFooter>
        {uploading && <Spinner />}
        <Button onClick={clear} variant="cancel">
          Cancel
        </Button>
        <Button
          onClick={handleUpload}
          variant="primary"
          disabled={listTorrentLoading || uploading || selectedFiles.size == 0}
        >
          OK
        </Button>
      </ModalFooter>
    </Modal>
  );
};
