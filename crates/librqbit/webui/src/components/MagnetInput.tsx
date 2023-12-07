import { useState } from "react";
import { UploadButton } from "./UploadButton";
import { UrlPromptModal } from "./UrlPromptModal";

export const MagnetInput = () => {
  let [magnet, setMagnet] = useState<string | null>(null);

  let [showModal, setShowModal] = useState(false);

  return (
    <>
      <UploadButton
        variant="primary"
        buttonText="Add Torrent from Magnet / URL"
        onClick={() => {
          setShowModal(true);
        }}
        data={magnet}
        resetData={() => setMagnet(null)}
      />

      <UrlPromptModal
        show={showModal}
        setUrl={(url) => {
          setShowModal(false);
          setMagnet(url);
        }}
        cancel={() => {
          setShowModal(false);
          setMagnet(null);
        }}
      />
    </>
  );
};
