import { useContext, useMemo, useState } from "react";
import { FaDownload, FaFile } from "react-icons/fa";
import { GoSearch, GoX } from "react-icons/go";
import { APIContext } from "../../context";
import { useTorrentStore } from "../../stores/torrentStore";
import { useErrorStore } from "../../stores/errorStore";
import { formatBytes } from "../../helper/formatBytes";
import { IconButton } from "./IconButton";
import { Button } from "./Button";
import { Modal } from "../modal/Modal";
import { ModalBody } from "../modal/ModalBody";
import { ModalFooter } from "../modal/ModalFooter";
import { ErrorDetails, TorrentDetails } from "../../api-types";

export interface DownloadButtonProps {
  torrentId: number;
  torrentName?: string | null;
  disabled?: boolean;
}

interface ProcessedFile {
  id: number;
  filename: string;
  path: string;
  length: number;
}

function extractDownloadableFiles(
  details: TorrentDetails | null,
): ProcessedFile[] {
  if (!details || !details.files) {
    return [];
  }
  return details.files
    .map((file, id) => {
      if (file.attributes?.padding) {
        return null;
      }
      const filename =
        file.components && file.components.length > 0
          ? file.components[file.components.length - 1]
          : file.name;
      const path =
        file.components && file.components.length > 0
          ? file.components.join("/")
          : file.name;
      return {
        id,
        filename,
        path,
        length: file.length,
      };
    })
    .filter((f): f is ProcessedFile => f !== null);
}

function triggerBrowserDownload(url: string, filename?: string) {
  const link = document.createElement("a");
  link.href = url;
  if (filename) {
    link.download = filename;
  }
  document.body.appendChild(link);
  link.click();
  document.body.removeChild(link);
}

interface MultiFileDownloadModalProps {
  isOpen: boolean;
  onClose: () => void;
  torrentId: number;
  torrentName: string;
  files: ProcessedFile[];
}

export const MultiFileDownloadModal: React.FC<MultiFileDownloadModalProps> = ({
  isOpen,
  onClose,
  torrentId,
  torrentName,
  files,
}) => {
  const API = useContext(APIContext);
  const [search, setSearch] = useState("");

  const filteredFiles = useMemo(() => {
    if (!search.trim()) return files;
    const q = search.toLowerCase();
    return files.filter(
      (f) =>
        f.filename.toLowerCase().includes(q) ||
        f.path.toLowerCase().includes(q),
    );
  }, [files, search]);

  const totalBytes = useMemo(
    () => files.reduce((sum, f) => sum + f.length, 0),
    [files],
  );

  return (
    <Modal
      isOpen={isOpen}
      onClose={onClose}
      title={`Download Files - ${torrentName}`}
      className="sm:max-w-2xl flex flex-col max-h-[calc(100vh-2rem)] sm:max-h-[calc(100vh-4rem)]"
    >
      <ModalBody>
        <div className="flex flex-col gap-3">
          {/* Search bar when there are several files */}
          {files.length > 4 && (
            <div className="relative">
              <GoSearch className="absolute left-2.5 top-1/2 -translate-y-1/2 w-4 h-4 text-tertiary" />
              <input
                type="text"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="Filter files..."
                className="w-full pl-8 pr-8 py-1.5 text-sm bg-surface border border-divider rounded focus:outline-none focus:border-primary placeholder:text-tertiary"
              />
              {search && (
                <button
                  onClick={() => setSearch("")}
                  className="absolute right-2 top-1/2 -translate-y-1/2 p-0.5 text-tertiary hover:text-secondary cursor-pointer"
                  aria-label="Clear filter"
                >
                  <GoX className="w-4 h-4" />
                </button>
              )}
            </div>
          )}

          {/* Files count and size summary */}
          <div className="text-xs text-secondary flex justify-between items-center px-0.5">
            <span>
              {filteredFiles.length} of {files.length} files
            </span>
            <span>Total: {formatBytes(totalBytes)}</span>
          </div>

          {/* Scrollable files list */}
          <div className="max-h-[50vh] overflow-y-auto divide-y divide-divider border border-divider rounded bg-surface">
            {filteredFiles.length === 0 ? (
              <div className="p-4 text-center text-sm text-tertiary">
                No files match your search
              </div>
            ) : (
              filteredFiles.map((file) => {
                const streamUrl = API.getTorrentStreamUrl(
                  torrentId,
                  file.id,
                  file.filename,
                );
                return (
                  <div
                    key={file.id}
                    className="flex items-center justify-between p-2.5 hover:bg-surface-sunken/60 transition-colors gap-3"
                  >
                    <div className="flex items-center gap-2.5 min-w-0 flex-1">
                      <FaFile className="w-4 h-4 text-tertiary shrink-0" />
                      <div className="min-w-0 flex-1">
                        <p
                          className="text-sm font-medium truncate text-text"
                          title={file.filename}
                        >
                          {file.filename}
                        </p>
                        {file.path !== file.filename && (
                          <p
                            className="text-xs text-tertiary truncate"
                            title={file.path}
                          >
                            {file.path}
                          </p>
                        )}
                      </div>
                    </div>
                    <div className="flex items-center gap-3 shrink-0">
                      <span className="text-xs text-secondary whitespace-nowrap">
                        {formatBytes(file.length)}
                      </span>
                      <a
                        href={streamUrl ?? "#"}
                        download={file.filename}
                        className="inline-flex items-center gap-1.5 px-2.5 py-1 text-xs font-medium bg-primary text-white hover:bg-primary-hover rounded transition-colors"
                        title={`Download ${file.filename}`}
                      >
                        <FaDownload className="w-3 h-3" />
                        Download
                      </a>
                    </div>
                  </div>
                );
              })
            )}
          </div>
        </div>
      </ModalBody>
      <ModalFooter>
        <Button onClick={onClose} variant="secondary" size="sm">
          Close
        </Button>
      </ModalFooter>
    </Modal>
  );
};

export function useTorrentDownload(
  torrentId: number,
  torrentName?: string | null,
) {
  const API = useContext(APIContext);
  const cachedDetails = useTorrentStore((state) => state.getDetails(torrentId));
  const setDetails = useTorrentStore((state) => state.setDetails);
  const setAlert = useErrorStore((state) => state.setAlert);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const [loading, setLoading] = useState(false);
  const [modalOpen, setModalOpen] = useState(false);
  const [downloadFiles, setDownloadFiles] = useState<ProcessedFile[]>([]);

  const handleDownload = async () => {
    let details = cachedDetails;

    if (!details) {
      setLoading(true);
      try {
        details = await API.getTorrentDetails(torrentId);
        setDetails(torrentId, details);
      } catch (e) {
        setCloseableError({
          text: `Error loading details for torrent id=${torrentId}`,
          details: e as ErrorDetails,
        });
        setLoading(false);
        return;
      }
      setLoading(false);
    }

    const files = extractDownloadableFiles(details);

    if (files.length === 0) {
      setAlert({
        text: "No downloadable files",
        details: {
          text: "This torrent has no downloadable files or files are still initializing.",
        },
      });
      return;
    }

    if (files.length === 1) {
      // Single file -> trigger download directly
      const file = files[0];
      const streamUrl = API.getTorrentStreamUrl(
        torrentId,
        file.id,
        file.filename,
      );
      if (streamUrl) {
        triggerBrowserDownload(streamUrl, file.filename);
      }
      return;
    }

    // Multiple files -> open popup modal
    setDownloadFiles(files);
    setModalOpen(true);
  };

  return {
    loading,
    modalOpen,
    setModalOpen,
    downloadFiles,
    handleDownload,
    effectiveName:
      torrentName || cachedDetails?.name || `Torrent #${torrentId}`,
  };
}

export const DownloadIconButton: React.FC<DownloadButtonProps> = ({
  torrentId,
  torrentName,
  disabled,
}) => {
  const {
    loading,
    modalOpen,
    setModalOpen,
    downloadFiles,
    handleDownload,
    effectiveName,
  } = useTorrentDownload(torrentId, torrentName);

  return (
    <>
      <IconButton
        onClick={handleDownload}
        disabled={disabled || loading}
        title={loading ? "Loading files..." : "Download file"}
      >
        {loading ? (
          <svg
            className="animate-spin h-3.5 w-3.5 text-blue-500"
            xmlns="http://www.w3.org/2000/svg"
            fill="none"
            viewBox="0 0 24 24"
          >
            <circle
              className="opacity-25"
              cx="12"
              cy="12"
              r="10"
              stroke="currentColor"
              strokeWidth="4"
            ></circle>
            <path
              className="opacity-75"
              fill="currentColor"
              d="M4 12a8 8 0 018-8v8H4z"
            ></path>
          </svg>
        ) : (
          <FaDownload className="hover:text-blue-500" />
        )}
      </IconButton>
      {modalOpen && (
        <MultiFileDownloadModal
          isOpen={modalOpen}
          onClose={() => setModalOpen(false)}
          torrentId={torrentId}
          torrentName={effectiveName}
          files={downloadFiles}
        />
      )}
    </>
  );
};

export const DownloadTextButton: React.FC<DownloadButtonProps> = ({
  torrentId,
  torrentName,
  disabled,
}) => {
  const {
    loading,
    modalOpen,
    setModalOpen,
    downloadFiles,
    handleDownload,
    effectiveName,
  } = useTorrentDownload(torrentId, torrentName);

  return (
    <>
      <Button
        onClick={handleDownload}
        disabled={disabled || loading}
        variant="secondary"
        size="sm"
        className="shrink-0"
      >
        <FaDownload className="w-3 h-3" />
        {loading ? "Loading..." : "Download"}
      </Button>
      {modalOpen && (
        <MultiFileDownloadModal
          isOpen={modalOpen}
          onClose={() => setModalOpen(false)}
          torrentId={torrentId}
          torrentName={effectiveName}
          files={downloadFiles}
        />
      )}
    </>
  );
};

export const DownloadLink: React.FC<
  Pick<DownloadButtonProps, "torrentId" | "torrentName">
> = ({ torrentId, torrentName }) => {
  const {
    loading,
    modalOpen,
    setModalOpen,
    downloadFiles,
    handleDownload,
    effectiveName,
  } = useTorrentDownload(torrentId, torrentName);

  return (
    <>
      <a
        href="#"
        onClick={(e) => {
          e.preventDefault();
          handleDownload();
        }}
        className="text-blue-500 hover:underline cursor-pointer"
      >
        {loading ? "Loading..." : "Download"}
      </a>
      {modalOpen && (
        <MultiFileDownloadModal
          isOpen={modalOpen}
          onClose={() => setModalOpen(false)}
          torrentId={torrentId}
          torrentName={effectiveName}
          files={downloadFiles}
        />
      )}
    </>
  );
};
