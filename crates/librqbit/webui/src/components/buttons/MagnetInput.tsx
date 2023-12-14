import { useState } from "react";
import { CgLink } from "react-icons/cg";
import { UploadButton } from "./UploadButton";
import { Modal } from "../modal/Modal";
import { Button } from "./Button";
import { ModalBody } from "../modal/ModalBody";
import { ModalFooter } from "../modal/ModalFooter";
import { FormInput } from "../forms/FormInput";

export const MagnetInput = ({ className }: { className?: string }) => {
  const [magnet, setMagnet] = useState<string | null>(null);
  const [inputValue, setInputValue] = useState("");
  const [modalIsOpen, setModalIsOpen] = useState(false);

  const clear = () => {
    setModalIsOpen(false);
    setMagnet(null);
  };

  return (
    <>
      <UploadButton
        onClick={() => {
          setModalIsOpen(true);
        }}
        data={magnet}
        className={className}
        resetData={() => setMagnet(null)}
      >
        <CgLink color="blue" />
        <div>Add Torrent from Magnet / URL</div>
      </UploadButton>

      <Modal isOpen={modalIsOpen} onClose={clear} title="Add torrent">
        <ModalBody>
          <FormInput
            autoFocus
            value={inputValue}
            name="magnet"
            onChange={(e) => setInputValue(e.target.value)}
            placeholder="magnet:?xt=urn:btih:..."
            help="Enter magnet or HTTP(S) URL to the .torrent"
          />
        </ModalBody>

        <ModalFooter>
          <Button variant="cancel" onClick={clear}>
            Cancel
          </Button>
          <Button
            disabled={!inputValue}
            variant="primary"
            onClick={() => {
              setMagnet(inputValue);
              setInputValue("");
              setModalIsOpen(false);
            }}
          >
            Add
          </Button>
        </ModalFooter>
      </Modal>
    </>
  );
};
