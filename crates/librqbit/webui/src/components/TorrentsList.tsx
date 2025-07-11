import { TorrentIdWithStats, TorrentId } from "../api-types";
import { Spinner } from "./Spinner";
import { Torrent } from "./Torrent";

export const TorrentsList = (props: {
  torrents: Array<TorrentIdWithStats> | null;
  loading: boolean;
  compact: boolean;
}) => {
  return (
    <div className="flex flex-col gap-2 mx-2 pb-3 sm:px-7">
      {props.torrents === null ? (
        props.loading ? (
          <Spinner
            className="justify-center m-5"
            label="Loading torrent list"
          />
        ) : null
      ) : props.torrents.length === 0 ? (
        <p className="text-center">No existing torrents found.</p>
      ) : (
        props.torrents.map((t: TorrentIdWithStats) => (
          <Torrent key={t.id} torrent={t} compact={props.compact} />
        ))
      )}
    </div>
  );
};
