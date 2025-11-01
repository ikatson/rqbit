import { useContext, useState } from "react";
import {
  ErrorDetails as ApiErrorDetails,
} from "../../api-types";
import { APIContext } from "../../context";
import { ErrorWithLabel } from "../../rqbit-web";
import { Button } from "./Button";
import { FaPlay } from "react-icons/fa";

export const ResumeButton = ({ className }: { className?: string }) => {
  const [resumeAllTorrentsError, setResumeAllTorrentsError] =
    useState<ErrorWithLabel | null>(null);

  const API = useContext(APIContext);

  const resume = async () => {
    try {
      const listTorrentsResponse = await API.listTorrents();
      await Promise.all(
        listTorrentsResponse.torrents.map(async (torrent_id) => {
          await API.start(torrent_id.id)
        })
      );
    } catch (e) {
      setResumeAllTorrentsError({
        text: "Error resuming all torrents",
        details: e as ApiErrorDetails,
      });
    }
  };

  return (
    <>
      <Button onClick={resume} className={`group ${className}`}>
        <FaPlay className="text-blue-500 group-hover:text-white dark:text-white" />
        <div>Resume all</div>
      </Button>
    </>
  );
};
