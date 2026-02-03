import React, { useContext, useState, useEffect } from "react";
import { FaMagnet, FaCheck, FaSpinner } from "react-icons/fa";
import { APIContext } from "../context";
import { useErrorStore } from "../stores/errorStore";
import { generateMagnetLink } from "../helper/magnetUtils";
import { IconButton } from "./buttons/IconButton";
import { TorrentListItem } from "../api-types";

interface CopyMagnetButtonProps {
  torrent: TorrentListItem;
  className?: string;
  iconClassName?: string;
}

export const CopyMagnetButton: React.FC<CopyMagnetButtonProps> = ({
  torrent,
  className,
  iconClassName = "w-4 h-4",
}) => {
  const [status, setStatus] = useState<"idle" | "loading" | "copied">("idle");
  const API = useContext(APIContext);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  useEffect(() => {
    if (status === "copied") {
      const timer = setTimeout(() => setStatus("idle"), 2000);
      return () => clearTimeout(timer);
    }
  }, [status]);

  const handleCopy = async () => {
    // Note: IconButton handles e.stopPropagation(), so we don't need it here.
    if (status !== "idle") return;

    setStatus("loading");
    let trackers: string[] = [];

    try {
      const details = await API.getTorrentDetails(torrent.id);
      if (details.trackers) {
        trackers = details.trackers;
      }
    } catch (err) {
      console.warn("Could not fetch details for magnet, using basic link", err);
    }

    const magnet = generateMagnetLink(
      torrent.info_hash,
      torrent.name || "",
      trackers
    );

    try {
      if (navigator.clipboard) {
        await navigator.clipboard.writeText(magnet);
      } else {
        const textArea = document.createElement("textarea");
        textArea.value = magnet;
        document.body.appendChild(textArea);
        textArea.select();
        document.execCommand("copy");
        document.body.removeChild(textArea);
      }
      setStatus("copied");
    } catch (err) {
      console.error("Failed to copy magnet link", err);
      setCloseableError({
        text: "Failed to copy magnet link",
        details: err as any,
      });
      setStatus("idle");
    }
  };

  return (
    <IconButton
      onClick={handleCopy}
      title={status === "copied" ? "Copied!" : "Copy Magnet Link"}
      className={`${className} transition-all duration-200`}
    >
      {status === "loading" ? (
        <FaSpinner className={`${iconClassName} animate-spin text-tertiary`} />
      ) : status === "copied" ? (
        <FaCheck className={`${iconClassName} text-success`} />
      ) : (
        <FaMagnet
          className={`${iconClassName} text-secondary hover:text-blue-500 transition-colors`}
        />
      )}
    </IconButton>
  );
};
