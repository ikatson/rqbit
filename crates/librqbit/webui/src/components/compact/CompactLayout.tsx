import { TorrentId } from "../../api-types";
import { ActionBar } from "./ActionBar";
import { TorrentTable } from "./TorrentTable";
import { DetailPane } from "./DetailPane";

interface CompactLayoutProps {
  torrents: TorrentId[] | null;
  loading: boolean;
}

export const CompactLayout: React.FC<CompactLayoutProps> = ({ torrents, loading }) => {
  return (
    <div className="flex flex-col h-[calc(100vh-140px)]">
      <ActionBar />
      <div className="flex-1 overflow-auto">
        <TorrentTable torrents={torrents} loading={loading} />
      </div>
      <DetailPane />
    </div>
  );
};
