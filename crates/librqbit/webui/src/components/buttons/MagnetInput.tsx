import { useState } from "react";
import { CgLink } from "react-icons/cg";
import { UploadButton } from "./UploadButton";
import useModal from "../useModal";

export const MagnetInput = () => {
  const [magnet, setMagnet] = useState<string | null>(null);
  const [inputValue, setInputValue] = useState("");
  const [Modal, isOpen, openModal, closeModal] = useModal();

  return (
    <>
      <UploadButton
        variant="primary"
        buttonText="Add Torrent from Magnet / URL"
        icon={<CgLink color="blue" />}
        onClick={() => {
          openModal();
        }}
        data={magnet}
        resetData={() => setMagnet(null)}
      />

      <Modal isOpen={isOpen} closeModal={closeModal}>
        <h1 className="text-xl mb-2">Add Torrent</h1>
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
        <div className="flex gap-2 justify-end">
          <button
            className="p-2 rounded-lg hover:bg-red-100"
            onClick={closeModal}
          >
            Cancel
          </button>
          <button
            disabled={!inputValue}
            className="p-2 rounded-lg hover:bg-green-100 disabled:cursor-not-allowed"
            onClick={() => {
              setMagnet(inputValue);
              closeModal();
              setInputValue("");
            }}
          >
            Add
          </button>
        </div>
      </Modal>
    </>
  );
};
