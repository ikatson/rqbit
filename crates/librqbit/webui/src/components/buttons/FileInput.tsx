import { RefObject, useContext, useRef, useState } from "react";
import { UploadButton } from "./UploadButton";
import { CgFileAdd } from "react-icons/cg";
import { APIContext } from "../../context";
import { useTorrentStore } from "../../stores/torrentStore";

export const FileInput = ({ className }: { className?: string }) => {
  const inputRef = useRef<HTMLInputElement>(
    null,
  ) as RefObject<HTMLInputElement>;
  const [file, setFile] = useState<File | null>(null);

  const API = useContext(APIContext);

  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);

  const onFileChange = async () => {
    if (!inputRef?.current?.files) {
      return;
    }
    if (inputRef.current.files.length == 1) {
      const file = inputRef.current.files[0];
      setFile(file);
    } else {
      const files = inputRef.current.files;
      for (let i = 0; i < inputRef.current.files.length; i++) {
        const file = inputRef.current.files[i];
        API.uploadTorrent(file, { overwrite: true }).then(
          () => {
            console.log("uploaded file successfully");
            refreshTorrents();
          },
          (err) => {
            console.error("error uploading file", err);
          },
        );
      }
      reset();
    }
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
        multiple={true}
        accept=".torrent"
        onChange={onFileChange}
        hidden
      />
      <UploadButton
        onClick={onClick}
        data={file}
        resetData={reset}
        className={`group ${className}`}
      >
        <CgFileAdd className="text-blue-500 group-hover:text-white dark:text-white" />
        <div>Upload .torrent File</div>
      </UploadButton>
    </>
  );
};
