import { FaPause, FaPlay, FaTrash } from "react-icons/fa";
import { TorrentIdWithStats } from "../api-types";
import { IconButton } from "./buttons/IconButton";
import { useTorrentStore } from "../stores/torrentStore";
import { useContext, useState } from "react";
import { APIContext } from "../context";

export const TorrentActionsMulti: React.FC<{
  torrents: TorrentIdWithStats[];
}> = ({ torrents }) => {
  const refresh = useTorrentStore((state) => state.refreshTorrents);
  const [disabled, setDisabled] = useState(false);
  const API = useContext(APIContext);

  const pause = () => {
    setDisabled(true);
    Promise.all(
      torrents.map((t) => {
        console.log(t.id, t.stats.state);
        if (t.stats.state === "live") {
          API.pause(t.id).then(() => refresh());
        }
      }),
    ).finally(() => setDisabled(false));
  };

  const unpause = () => {
    setDisabled(true);
    Promise.all(
      torrents.map((t) => {
        console.log(t.id, t.stats.state);
        if (t.stats.state !== "live") {
          API.start(t.id).then(() => refresh());
        }
      }),
    ).finally(() => setDisabled(false));
  };

  return (
    <div className="flex gap-2 dark:text-slate-300">
      <IconButton onClick={unpause} disabled={disabled}>
        <FaPlay className="hover:text-green-600" />
      </IconButton>

      <IconButton onClick={pause} disabled={disabled}>
        <FaPause className="hover:text-amber-500" />
      </IconButton>
      {/* <IconButton onClick={startDeleting} disabled={disabled}>
        <FaTrash className="hover:text-red-500" />
      </IconButton> */}
    </div>
  );
};
