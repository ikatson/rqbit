import { useState } from "react";
import { CgLink } from "react-icons/cg";
import { UploadButton } from "./UploadButton";
import { Modal } from "../modal/Modal";
import { Button } from "./Button";
import { ModalBody } from "../modal/ModalBody";
import { ModalFooter } from "../modal/ModalFooter";

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
          <p className="mb-2 text-sm text-gray-500 italic">
            Enter magnet or HTTP(S) URL to the .torrent
          </p>
          <input
            autoFocus
            className="w-full border rounded-md p-2 my-2"
            value={inputValue}
            onChange={(e) => setInputValue(e.target.value)}
            type="text"
            placeholder="magnet:?xt=urn:btih:..."
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
