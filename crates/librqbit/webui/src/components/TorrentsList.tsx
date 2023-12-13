import { TorrentId } from "../api-types";
import { Spinner } from "./Spinner";
import { Torrent } from "./Torrent";

export const TorrentsList = (props: {
  torrents: Array<TorrentId> | null;
  loading: boolean;
}) => {
  if (props.torrents === null && props.loading) {
    return <Spinner />;
  }
  // The app either just started, or there was an error loading torrents.
  if (props.torrents === null) {
    return;
  }

  if (props.torrents.length === 0) {
    return (
      <div className="text-center">
        <p>No existing torrents found.</p>
      </div>
    );
  }
  return (
    <div className="flex flex-col gap-2">
      {props.torrents.map((t: TorrentId) => (
        <Torrent id={t.id} key={t.id} torrent={t} />
      ))}
    </div>
  );
};
