import { useContext, useState } from "react";
import {
  ErrorDetails as ApiErrorDetails,
} from "../../api-types";
import { APIContext } from "../../context";
import { ErrorWithLabel } from "../../rqbit-web";
import { Button } from "./Button";
import { FaPause, FaPlay } from "react-icons/fa";

export const PauseButton = ({ className }: { className?: string }) => {
  const [pauseAllTorrentsError, setPauseAllTorrentsError] =
    useState<ErrorWithLabel | null>(null);

  const API = useContext(APIContext);

  const pause = async () => {
    try {
      const listTorrentsResponse = await API.listTorrents();
      await Promise.all(
        listTorrentsResponse.torrents.map(async (torrent_id) => {
          await API.pause(torrent_id.id);
        })
      );
    } catch (e) {
      setPauseAllTorrentsError({
        text: "Error pausing all torrents",
        details: e as ApiErrorDetails,
      });
    }
  };

  return (
    <>
      <Button onClick={pause} className={`group ${className}`}>
        <FaPause className="text-blue-500 group-hover:text-white dark:text-white" />
        <div>Pause all</div>
      </Button>
    </>
  );
};
