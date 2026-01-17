import { useTorrentStore } from "../../stores/torrentStore";
import { Modal } from "./Modal";
import { ActionBar } from "../compact/ActionBar";
import { DetailPane } from "../compact/DetailPane";

interface TorrentDetailsModalProps {
  torrentId: number;
  isOpen: boolean;
  onClose: () => void;
}

export const TorrentDetailsModal: React.FC<TorrentDetailsModalProps> = ({
  torrentId,
  isOpen,
  onClose,
}) => {
  const torrent = useTorrentStore((state) =>
    state.torrents?.find((t) => t.id === torrentId),
  );

  const title = torrent?.name ?? `Torrent #${torrentId}`;

  return (
    <Modal
      isOpen={isOpen}
      onClose={onClose}
      title={title}
      className="sm:max-w-4xl flex flex-col max-h-[calc(100vh-2rem)] sm:max-h-[calc(100vh-4rem)]"
    >
      <ActionBar hideFilters />
      <div className="flex-1 min-h-0">
        <DetailPane />
      </div>
    </Modal>
  );
};
