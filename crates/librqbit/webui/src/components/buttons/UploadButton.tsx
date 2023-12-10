import { ReactNode, useContext, useEffect, useState } from "react";
import {
  AddTorrentResponse,
  ErrorDetails as ApiErrorDetails,
} from "../../api-types";
import { APIContext } from "../../context";
import { ErrorWithLabel } from "../../rqbit-web";
import { FileSelectionModal } from "../modals/FileSelectionModal";

export const UploadButton: React.FC<{
  buttonText: string;
  onClick: () => void;
  data: string | File | null;
  resetData: () => void;
  variant: string;
  icon: ReactNode;
}> = ({ buttonText, onClick, data, resetData, variant, icon }) => {
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
        const response = await API.uploadTorrent(data, { list_only: true });
        setListTorrentResponse(response);
      } catch (e) {
        setListTorrentError({
          text: "Error listing torrent files",
          details: e as ApiErrorDetails,
        });
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
      <button
        onClick={onClick}
        className="inline-flex gap-1 border rounded-lg hover:bg-blue-600 transition-colors duration-500 hover:text-white items-center p-1"
      >
        {icon}
        {buttonText}
      </button>

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
