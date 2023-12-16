import { useState } from "react";
import { TorrentId } from "../api-types";
import { Spinner } from "./Spinner";
import { Torrent } from "./Torrent";

export const TorrentsList = (props: {
  torrents: Array<TorrentId> | null;
  loading: boolean;
}) => {
  const [selectedRows, setSelectedRows] = useState<TorrentId[]>([]);
  const handleTorrentClick = (torrent: TorrentId) => {
    if (selectedRows.includes(torrent)) {
      setSelectedRows((prev) => prev.filter((t) => t !== torrent));
    } else {
      setSelectedRows((prev) => [...prev, torrent]);
    }
  };
  return (
    <div className="flex flex-col gap-2 mx-2 pb-3 sm:px-7">
      {props.torrents === null ? (
        props.loading ? (
          <Spinner label="Loading torrent list" />
        ) : null
      ) : props.torrents.length === 0 ? (
        <p className="text-center">No existing torrents found.</p>
      ) : (
        props.torrents.map((t: TorrentId) => (
          <div
            className={selectedRows.includes(t) ? "bg-red-300" : "bg-none"}
            onClick={() => handleTorrentClick(t)}
          >
            <Torrent id={t.id} key={t.id} torrent={t} />
          </div>
        ))
      )}
    </div>
  );
};
