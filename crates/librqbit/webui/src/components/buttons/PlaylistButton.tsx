import { useContext } from "react";
import { FaClipboardList } from "react-icons/fa";
import { APIContext } from "../../context";
import { useErrorStore } from "../../stores/errorStore";
import { Button } from "./Button";
import { IconButton } from "./IconButton";

interface PlaylistButtonProps {
  torrentId: number;
  disabled?: boolean;
}

function usePlaylistClipboard(torrentId: number) {
  const API = useContext(APIContext);
  const setAlert = useErrorStore((state) => state.setAlert);
  const playlistUrl = API.getPlaylistUrl(torrentId);

  const copyPlaylistUrlToClipboard = async () => {
    if (!playlistUrl) {
      return;
    }
    try {
      await navigator.clipboard.writeText(playlistUrl);
    } catch {
      setAlert({
        text: "Copy playlist URL",
        details: {
          text: (
            <>
              <p>
                Copy{" "}
                <a href={playlistUrl} className="text-blue-500">
                  playlist URL
                </a>{" "}
                to clipboard and paste into e.g. VLC to play.
              </p>
            </>
          ),
        },
      });
      return;
    }

    setAlert({
      text: "Copied",
      details: {
        text: "Playlist URL copied to clipboard. Paste into e.g. VLC to play.",
      },
    });
  };

  return { playlistUrl, copyPlaylistUrlToClipboard };
}

export const PlaylistIconButton: React.FC<PlaylistButtonProps> = ({
  torrentId,
  disabled,
}) => {
  const { playlistUrl, copyPlaylistUrlToClipboard } =
    usePlaylistClipboard(torrentId);

  return (
    <IconButton
      href={playlistUrl ?? "#"}
      onClick={copyPlaylistUrlToClipboard}
      disabled={disabled || !playlistUrl}
      title="Copy playlist URL"
    >
      <FaClipboardList className="hover:text-green-500" />
    </IconButton>
  );
};

export const PlaylistTextButton: React.FC<PlaylistButtonProps> = ({
  torrentId,
  disabled,
}) => {
  const { playlistUrl, copyPlaylistUrlToClipboard } =
    usePlaylistClipboard(torrentId);

  return (
    <Button
      onClick={copyPlaylistUrlToClipboard}
      disabled={disabled || !playlistUrl}
      variant="secondary"
      size="sm"
      className="shrink-0"
    >
      <FaClipboardList className="w-3 h-3" />
      Playlist
    </Button>
  );
};

export const PlaylistLink: React.FC<Pick<PlaylistButtonProps, "torrentId">> = ({
  torrentId,
}) => {
  const { playlistUrl, copyPlaylistUrlToClipboard } =
    usePlaylistClipboard(torrentId);

  if (!playlistUrl) {
    return <span className="text-tertiary">Unavailable</span>;
  }

  return (
    <a
      href={playlistUrl}
      onClick={(e) => {
        e.preventDefault();
        copyPlaylistUrlToClipboard();
      }}
      className="text-blue-500 hover:underline cursor-pointer"
    >
      Copy playlist URL
    </a>
  );
};
