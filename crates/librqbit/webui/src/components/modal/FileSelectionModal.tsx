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
import { FormCheckbox } from "../forms/FormCheckbox";
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
  const [skipInitialCheck, setSkipInitialCheck] = useState(false);
  const [outputFolder, setOutputFolder] = useState<string>("");
  const [overwrite, setOverwrite] = useState(true);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const API = useContext(APIContext);

  useEffect(() => {
    setSelectedFiles(
      new Set(
        listTorrentResponse?.details.files.flatMap((file, idx) => {
          if (file.attributes.padding) {
            return [];
          } else {
            return [idx];
          }
        }),
      ),
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
      overwrite: overwrite,
      only_files: Array.from(selectedFiles),
      initial_peers: initialPeers,
      output_folder: outputFolder,
      skip_initial_check: skipInitialCheck,
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
        },
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
              torrentDetails={listTorrentResponse.details}
              torrentStats={null}
            />
          </Fieldset>

    <Fieldset label="Options">
            <FormCheckbox
              label="Overwrite existing files"
              checked={overwrite}
              onChange={() => setOverwrite(!overwrite)}
              help="Allow writing to existing files (required for resuming)"
              name="overwrite"
            />
            <FormCheckbox
              label="Skip hash check"
              checked={skipInitialCheck}
              onChange={() => setSkipInitialCheck(!skipInitialCheck)}
              help="Trust that existing files are correct. Useful for large torrents."
              name="skip_initial_check"
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
          disabled={listTorrentLoading || uploading || selectedFiles.size == 0}
        >
          OK
        </Button>
      </ModalFooter>
    </Modal>
  );
};
