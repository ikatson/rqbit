import { useContext, useEffect, useState } from "react";
import { FileListInput } from "./FileListInput";
import { ErrorDetails, TorrentDetails, TorrentIdWithStats } from "../api-types";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { APIContext } from "../context";
import { useErrorStore } from "../stores/errorStore";
import { Spinner } from "./Spinner";

export const ManagedTorrentFileListInput: React.FC<{
  torrent: TorrentIdWithStats;
}> = ({ torrent }) => {
  const [detailsResponse, setDetailsResponse] = useState<TorrentDetails | null>(
    null,
  );
  const API = useContext(APIContext);
  const [selectedFiles, setSelectedFiles] = useState<Set<number>>(new Set());
  const [savingSelectedFiles, setSavingSelectedFiles] = useState(false);

  // Update details once then when asked for.
  useEffect(() => {
    return loopUntilSuccess(async () => {
      await API.getTorrentDetails(torrent.id).then(setDetailsResponse);
    }, 1000);
  }, [torrent.id]);

  // Update selected files whenever details are updated.
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

  let setCloseableError = useErrorStore((state) => state.setCloseableError);

  const updateSelectedFiles = (selectedFiles: Set<number>) => {
    setSavingSelectedFiles(true);
    API.updateOnlyFiles(torrent.id, Array.from(selectedFiles))
      .then(
        () => {
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
    return (
      <div className="flex justify-center p-2">
        <Spinner label="Loading file list" />
      </div>
    );
  }

  return (
    <FileListInput
      torrentId={torrent.id}
      torrentDetails={detailsResponse}
      torrentStats={torrent.stats}
      selectedFiles={selectedFiles}
      setSelectedFiles={updateSelectedFiles}
      disabled={savingSelectedFiles}
      allowStream
      showProgressBar
    />
  );
};
