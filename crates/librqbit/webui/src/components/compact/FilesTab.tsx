import { useContext, useEffect, useState } from "react";
import { TorrentDetails, TorrentStats, ErrorDetails } from "../../api-types";
import { APIContext } from "../../context";
import { FileListInput } from "../FileListInput";
import { useErrorStore } from "../../stores/errorStore";

interface FilesTabProps {
  torrentId: number;
  detailsResponse: TorrentDetails | null;
  statsResponse: TorrentStats | null;
  onRefresh?: () => void;
}

export const FilesTab: React.FC<FilesTabProps> = ({
  torrentId,
  detailsResponse,
  statsResponse,
  onRefresh,
}) => {
  const [selectedFiles, setSelectedFiles] = useState<Set<number>>(new Set());
  const [savingSelectedFiles, setSavingSelectedFiles] = useState(false);

  const API = useContext(APIContext);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  useEffect(() => {
    setSelectedFiles(
      new Set<number>(
        detailsResponse?.files
          .map((f, id) => ({ f, id }))
          .filter(({ f }) => f.included)
          .map(({ id }) => id) ?? [],
      ),
    );
  }, [detailsResponse]);

  const updateSelectedFiles = (selectedFiles: Set<number>) => {
    setSavingSelectedFiles(true);
    API.updateOnlyFiles(torrentId, Array.from(selectedFiles))
      .then(
        () => {
          onRefresh?.();
          setCloseableError(null);
        },
        (e) => {
          setCloseableError({
            text: "Error configuring torrent",
            details: e as ErrorDetails,
          });
        },
      )
      .finally(() => setSavingSelectedFiles(false));
  };

  if (!detailsResponse) {
    return <div className="p-4 text-tertiary">Loading...</div>;
  }

  return (
    <div className="p-2 text-sm">
      <FileListInput
        torrentId={torrentId}
        torrentDetails={detailsResponse}
        torrentStats={statsResponse}
        selectedFiles={selectedFiles}
        setSelectedFiles={updateSelectedFiles}
        disabled={savingSelectedFiles}
        allowStream
        showProgressBar
      />
    </div>
  );
};
