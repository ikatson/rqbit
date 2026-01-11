import { useContext, useEffect, useState } from "react";
import { TorrentDetails, TorrentStats, ErrorDetails } from "../../api-types";
import { APIContext, RefreshTorrentStatsContext } from "../../context";
import { FileListInput } from "../FileListInput";
import { useErrorStore } from "../../stores/errorStore";

interface FilesTabProps {
  torrentId: number;
  detailsResponse: TorrentDetails | null;
  statsResponse: TorrentStats | null;
}

export const FilesTab: React.FC<FilesTabProps> = ({
  torrentId,
  detailsResponse,
  statsResponse,
}) => {
  const [selectedFiles, setSelectedFiles] = useState<Set<number>>(new Set());
  const [savingSelectedFiles, setSavingSelectedFiles] = useState(false);

  const API = useContext(APIContext);
  const refreshCtx = useContext(RefreshTorrentStatsContext);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  useEffect(() => {
    setSelectedFiles(
      new Set<number>(
        detailsResponse?.files
          .map((f, id) => ({ f, id }))
          .filter(({ f }) => f.included)
          .map(({ id }) => id) ?? []
      )
    );
  }, [detailsResponse]);

  const updateSelectedFiles = (selectedFiles: Set<number>) => {
    setSavingSelectedFiles(true);
    API.updateOnlyFiles(torrentId, Array.from(selectedFiles))
      .then(
        () => {
          refreshCtx.refresh();
          setCloseableError(null);
        },
        (e) => {
          setCloseableError({
            text: "Error configuring torrent",
            details: e as ErrorDetails,
          });
        }
      )
      .finally(() => setSavingSelectedFiles(false));
  };

  if (!detailsResponse) {
    return (
      <div className="p-4 text-gray-400 dark:text-slate-500">
        Loading...
      </div>
    );
  }

  return (
    <div className="p-2">
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
