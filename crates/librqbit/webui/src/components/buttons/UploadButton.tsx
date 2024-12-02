import { ReactNode, useContext, useEffect, useState } from "react";
import {
  AddTorrentResponse,
  ErrorDetails as ApiErrorDetails,
} from "../../api-types";
import { APIContext } from "../../context";
import { ErrorWithLabel } from "../../rqbit-web";
import { FileSelectionModal } from "../modal/FileSelectionModal";
import { Button } from "./Button";

export const UploadButton: React.FC<{
  onClick: () => void;
  data: string | File | null;
  resetData: () => void;
  children: ReactNode;
  className?: string;
}> = ({ onClick, data, resetData, children, className }) => {
  const [loading, setLoading] = useState(false);
  const [listTorrentResponse, setListTorrentResponse] =
    useState<AddTorrentResponse | null>(null);
  const [listTorrentError, setListTorrentError] =
    useState<ErrorWithLabel | null>(null);
  const API = useContext(APIContext);

  // Get the torrent file list if there's data.
  useEffect(() => {
    if (data === null) {
      return;
    }

    let t = setTimeout(async () => {
      setLoading(true);
      try {
        const response = await API.uploadTorrent(data, { list_only: true }, 2_000);
        setListTorrentResponse(response);
      } catch (e: unknown) {
        let error = e as ApiErrorDetails;
        if (error.timedOut) {
          setListTorrentResponse(null);
          // Timeout is not an error for a listOnly request
          setListTorrentError(null);
        } else {
          setListTorrentError({
            text: "Error listing torrent files",
            details: error,
          });
        }
      } finally {
        setLoading(false);
      }
    }, 0);
    return () => clearTimeout(t);
  }, [data]);

  const clear = () => {
    resetData();
    setListTorrentError(null);
    setListTorrentResponse(null);
    setLoading(false);
  };

  return (
    <>
      <Button onClick={onClick} className={className}>
        {children}
      </Button>

      {data && (
        <FileSelectionModal
          onHide={clear}
          listTorrentError={listTorrentError}
          listTorrentResponse={listTorrentResponse}
          data={data}
          listTorrentLoading={loading}
        />
      )}
    </>
  );
};
