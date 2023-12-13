import { useContext, useEffect, useState } from "react";
import { AddTorrentResponse, AddTorrentOptions } from "../../api-types";
import { AppContext, APIContext } from "../../context";
import { ErrorComponent } from "../ErrorComponent";
import { formatBytes } from "../../helper/formatBytes";
import { ErrorWithLabel } from "../../rqbit-web";
import { Spinner } from "../Spinner";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";
import { Button } from "../buttons/Button";
import { FormCheckbox } from "../forms/FormCheckbox";
import { Fieldset } from "../forms/Fieldset";
import { FormInput } from "../forms/FormInput";
import { Form } from "../forms/Form";

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
          <Fieldset className="mb-4" label="Pick the files to download">
            {listTorrentResponse.details.files.map((file, index) => (
              <FormCheckbox
                key={index}
                label={`${file.name}  (${formatBytes(file.length)})`}
                checked={selectedFiles.includes(index)}
                onChange={() => handleToggleFile(index)}
                name={`check-${index}`}
              />
            ))}
          </Fieldset>
          <Fieldset label="Options">
            <FormInput
              label="Output folder"
              name="output_folder"
              inputType="text"
              help="Some help text"
              value={outputFolder}
              onChange={(e) => setOutputFolder(e.target.value)}
            />

            <FormCheckbox
              label="Increase timeouts"
              checked={unpopularTorrent}
              onChange={() => setUnpopularTorrent(!unpopularTorrent)}
              help="This might be useful for unpopular torrents with few peers. It will slow down fast torrents though."
              name="increase_timeouts"
            />
          </Fieldset>
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
          disabled={
            listTorrentLoading || uploading || selectedFiles.length == 0
          }
        >
          OK
        </Button>
      </ModalFooter>
    </Modal>
  );
};
