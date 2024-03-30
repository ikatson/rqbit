import React, { useContext, useState } from "react";
import {
  AddTorrentResponse,
  ErrorDetails,
  TorrentDetails,
} from "../../api-types";
import { FileListInput } from "../FileListInput";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";
import { Button } from "../buttons/Button";
import { Spinner } from "../Spinner";
import { APIContext, RefreshTorrentStatsContext } from "../../context";
import { ErrorComponent } from "../ErrorComponent";
import { ErrorWithLabel } from "../../stores/errorStore";

export const TorrentSettingsModal: React.FC<{
  id: number;
  show: boolean;
  onHide: () => void;
  details: TorrentDetails;
}> = ({ id, show, onHide, details }) => {
  let initialSelectedFiles = new Set<number>();

  let refreshCtx = useContext(RefreshTorrentStatsContext);

  details.files.forEach((f, i) => {
    if (f.included) {
      initialSelectedFiles.add(i);
    }
  });

  const API = useContext(APIContext);

  const [selectedFiles, setSelectedFiles] =
    useState<Set<number>>(initialSelectedFiles);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<ErrorWithLabel | null>(null);

  const close = () => {
    setSelectedFiles(initialSelectedFiles);
    onHide();
  };

  const handleSave = () => {
    setSaving(true);
    API.updateOnlyFiles(id, Array.from(selectedFiles)).then(
      () => {
        setSaving(false);
        refreshCtx.refresh();
        close();
        setError(null);
      },
      (e) => {
        setSaving(false);
        setError({
          text: "Error configuring torrent",
          details: e as ErrorDetails,
        });
      },
    );
  };

  return (
    <Modal isOpen={show} onClose={close} title="Configure torrent">
      <ModalBody>
        <ErrorComponent error={error}></ErrorComponent>
        <FileListInput
          torrentDetails={details}
          selectedFiles={selectedFiles}
          setSelectedFiles={setSelectedFiles}
        />
      </ModalBody>

      <ModalFooter>
        {saving && <Spinner />}
        <Button onClick={close} variant="cancel">
          Cancel
        </Button>
        <Button
          onClick={handleSave}
          variant="primary"
          disabled={saving || selectedFiles.size == 0}
        >
          OK
        </Button>
      </ModalFooter>
    </Modal>
  );
};
