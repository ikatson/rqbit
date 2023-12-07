import { Spinner } from "react-bootstrap";
import { TorrentId } from "../api-types";
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
        <p>No existing torrents found. Add them through buttons below.</p>
      </div>
    );
  }
  return (
    <div style={{ fontSize: "smaller" }}>
      {props.torrents.map((t: TorrentId) => (
        <Torrent id={t.id} key={t.id} torrent={t} />
      ))}
    </div>
  );
};
