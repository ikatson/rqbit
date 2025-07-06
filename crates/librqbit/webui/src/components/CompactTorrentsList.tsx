import { TorrentIdWithStats, TorrentDetails } from "../api-types";
import { Spinner } from "./Spinner";
import { Torrent } from "./Torrent";
import { useContext, useEffect, useState, useMemo, useCallback } from "react";
import { APIContext } from "../context";
import { loopUntilSuccess } from "../helper/loopUntilSuccess";
import { torrentDisplayName } from "../helper/getTorrentDisplayName";
import { formatBytes } from "../helper/formatBytes";
import { getCompletionETA } from "../helper/getCompletionETA";
import { Speed } from "./Speed";
import { FaArrowUp, FaArrowDown } from "react-icons/fa";

export const CompactTorrentsList = (props: {
  torrents: Array<TorrentIdWithStats> | null;
  loading: boolean;
  onTorrentClick: (id: number) => void;
  selectedTorrent: number | null;
}) => {
  type SortColumn =
    | "id"
    | "name"
    | "progress"
    | "speed"
    | "eta"
    | "peers"
    | "size";

  const [sortColumn, setSortColumn] = useState<SortColumn>("name");
  const [sortDirectionIsAsc, setSortDirectionIsAsc] = useState<boolean>(true);

  const API = useContext(APIContext);

  const sortedTorrentData = useMemo(() => {
    if (!props.torrents) return [];

    const sortableData = [...props.torrents];

    sortableData.sort((a, b) => {
      const getSortValue = (
        torrent: TorrentIdWithStats,
        column: SortColumn,
      ) => {
        switch (column) {
          case "id":
            return torrent.id;
          case "name":
            return torrent.name || "";
          case "progress":
            const progress = torrent.stats?.progress_bytes || 0;
            const total = torrent.stats?.total_bytes || 1;
            return progress / total;
          case "speed":
            return torrent.stats?.live?.download_speed.mbps || 0;
          case "eta":
            return (
              torrent.stats?.live?.time_remaining?.duration?.secs || Infinity
            );
          case "peers":
            return torrent.stats?.live?.snapshot.peer_stats.live || 0;
          case "size":
            return torrent.stats?.total_bytes || 0;
        }
      };

      const valueA = getSortValue(a, sortColumn);
      const valueB = getSortValue(b, sortColumn);

      let compareValue = 0;
      if (typeof valueA === "string" && typeof valueB === "string") {
        compareValue = valueA.localeCompare(valueB);
      } else if (typeof valueA === "number" && typeof valueB === "number") {
        compareValue = valueA - valueB;
      }
      return sortDirectionIsAsc ? compareValue : -compareValue;
    });
    return sortableData;
  }, [props.torrents, sortColumn, sortDirectionIsAsc]);

  const handleSort = useCallback(
    (newColumn: SortColumn) => {
      if (sortColumn === newColumn) {
        setSortDirectionIsAsc(!sortDirectionIsAsc);
      } else {
        setSortColumn(newColumn);
        setSortDirectionIsAsc(["name", "id"].indexOf(sortColumn) !== -1);
      }
    },
    [sortColumn],
  );

  const getSortIndicator = (column: SortColumn) => {
    if (sortColumn === column) {
      return sortDirectionIsAsc ? (
        <FaArrowUp className="inline ml-1" />
      ) : (
        <FaArrowDown className="inline ml-1" />
      );
    }
    return null;
  };

  const thClassNames =
    "px-2 py-1 text-left text-xs font-medium text-gray-500 uppercase tracking-wider";
  const thClickableClassNames = `${thClassNames} cursor-pointer`;

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
        <div className="overflow-x-auto">
          <table className="min-w-full divide-y divide-gray-200 dark:divide-gray-700">
            <thead className="bg-gray-50 dark:bg-gray-800">
              <tr>
                <th
                  className={thClickableClassNames}
                  onClick={() => handleSort("id")}
                >
                  ID{getSortIndicator("id")}
                </th>
                <th className={thClassNames}></th>
                <th
                  className={thClickableClassNames}
                  onClick={() => handleSort("name")}
                >
                  Name{getSortIndicator("name")}
                </th>
                <th
                  className={thClickableClassNames}
                  onClick={() => handleSort("progress")}
                >
                  Progress{getSortIndicator("progress")}
                </th>
                <th
                  className={thClickableClassNames}
                  onClick={() => handleSort("speed")}
                >
                  Speed{getSortIndicator("speed")}
                </th>
                <th
                  className={thClickableClassNames}
                  onClick={() => handleSort("eta")}
                >
                  ETA{getSortIndicator("eta")}
                </th>
                <th
                  className={thClickableClassNames}
                  onClick={() => handleSort("peers")}
                >
                  Peers{getSortIndicator("peers")}
                </th>
                <th
                  className={thClickableClassNames}
                  onClick={() => handleSort("size")}
                >
                  Size{getSortIndicator("size")}
                </th>
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200 dark:bg-gray-900 dark:divide-gray-700">
              {sortedTorrentData.map((t) => (
                <Torrent
                  key={t.id}
                  torrent={t}
                  compact
                  onClick={() => props.onTorrentClick(t.id)}
                  selected={t.id === props.selectedTorrent}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
};
