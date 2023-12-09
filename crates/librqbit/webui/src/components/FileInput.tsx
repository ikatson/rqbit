import { RefObject, useRef, useState } from "react";
import { UploadButton } from "./UploadButton";
import { CgFileAdd } from "react-icons/cg";

export const FileInput = () => {
  const inputRef = useRef<HTMLInputElement>() as RefObject<HTMLInputElement>;
  const [file, setFile] = useState<File | null>(null);

  const onFileChange = async () => {
    if (!inputRef?.current?.files) {
      return;
    }
    const file = inputRef.current.files[0];
    setFile(file);
  };

  const reset = () => {
    if (!inputRef?.current) {
      return;
    }
    inputRef.current.value = "";
    setFile(null);
  };

  const onClick = () => {
    if (!inputRef?.current) {
      return;
    }
    inputRef.current.click();
  };

  return (
    <>
      <input
        type="file"
        ref={inputRef}
        accept=".torrent"
        onChange={onFileChange}
        className="d-none"
      />
      <UploadButton
        variant="secondary"
        buttonText="Upload .torrent File"
        onClick={onClick}
        data={file}
        resetData={reset}
        icon={<CgFileAdd />}
      />
    </>
  );
};
