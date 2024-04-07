import { useMemo, useState } from "react";
import { TorrentDetails, TorrentStats } from "../api-types";
import { FormCheckbox } from "./forms/FormCheckbox";
import { CiSquarePlus, CiSquareMinus } from "react-icons/ci";
import { IconButton } from "./buttons/IconButton";
import { formatBytes } from "../helper/formatBytes";
import { ProgressBar } from "./ProgressBar";
import sortBy from "lodash.sortby";

type TorrentFileForCheckbox = {
  id: number;
  filename: string;
  pathComponents: string[];
  length: number;
  have_bytes: number;
};

type FileTree = {
  id: string;
  name: string;
  dirs: FileTree[];
  files: TorrentFileForCheckbox[];
};

const newFileTree = (
  torrentDetails: TorrentDetails,
  stats: TorrentStats | null,
): FileTree => {
  const newFileTreeInner = (
    name: string,
    id: string,
    files: TorrentFileForCheckbox[],
    depth: number,
  ): FileTree => {
    let directFiles: TorrentFileForCheckbox[] = [];
    let groups: FileTree[] = [];
    let groupsByName: { [key: string]: TorrentFileForCheckbox[] } = {};

    const getGroup = (prefix: string): TorrentFileForCheckbox[] => {
      groupsByName[prefix] = groupsByName[prefix] || [];
      return groupsByName[prefix];
    };

    files.forEach((file: TorrentFileForCheckbox) => {
      if (depth == file.pathComponents.length - 1) {
        directFiles.push(file);
        return;
      }
      getGroup(file.pathComponents[0]).push(file);
    });

    directFiles = sortBy(directFiles, (f) => f.filename);

    let sortedGroupsByName = sortBy(
      Object.entries(groupsByName),
      ([k, _]) => k,
    );

    let childId = 0;
    for (const [key, value] of sortedGroupsByName) {
      groups.push(newFileTreeInner(key, id + "." + childId, value, depth + 1));
      childId += 1;
    }
    return {
      name,
      id,
      dirs: groups,
      files: directFiles,
    };
  };

  return newFileTreeInner(
    "",
    "filetree-root",
    torrentDetails.files.map((file, id) => {
      return {
        id,
        filename: file.components[file.components.length - 1],
        pathComponents: file.components,
        length: file.length,
        have_bytes: stats ? stats.file_progress[id] ?? 0 : 0,
      };
    }),
    0,
  );
};

const FileTreeComponent: React.FC<{
  tree: FileTree;
  torrentDetails: TorrentDetails;
  torrentStats: TorrentStats | null;
  selectedFiles: Set<number>;
  setSelectedFiles: (_: Set<number>) => void;
  initialExpanded: boolean;
  showProgressBar?: boolean;
  disabled?: boolean;
}> = ({
  tree,
  selectedFiles,
  setSelectedFiles,
  initialExpanded,
  torrentDetails,
  torrentStats,
  showProgressBar,
  disabled,
}) => {
  let [expanded, setExpanded] = useState(initialExpanded);
  let children = useMemo(() => {
    let getAllChildren = (tree: FileTree): number[] => {
      let children = tree.dirs.flatMap(getAllChildren);
      children.push(...tree.files.map((file) => file.id));
      return children;
    };
    return getAllChildren(tree);
  }, [tree]);

  const handleToggleTree: React.ChangeEventHandler<HTMLInputElement> = (e) => {
    if (e.target.checked) {
      let copy = new Set(selectedFiles);
      children.forEach((c) => copy.add(c));
      setSelectedFiles(copy);
    } else {
      let copy = new Set(selectedFiles);
      children.forEach((c) => copy.delete(c));
      setSelectedFiles(copy);
    }
  };

  const handleToggleFile = (toggledId: number) => {
    if (selectedFiles.has(toggledId)) {
      let copy = new Set(selectedFiles);
      copy.delete(toggledId);
      setSelectedFiles(copy);
    } else {
      let copy = new Set(selectedFiles);
      copy.add(toggledId);
      setSelectedFiles(copy);
    }
  };

  const getTotalSelectedFiles = () => {
    return children.filter((c) => selectedFiles.has(c)).length;
  };

  const getTotalSelectedBytes = () => {
    return children
      .filter((c) => selectedFiles.has(c))
      .map((c) => torrentDetails.files[c].length)
      .reduce((a, b) => a + b, 0);
  };

  return (
    <>
      <div className="flex items-center">
        <IconButton onClick={() => setExpanded(!expanded)}>
          {expanded ? <CiSquareMinus /> : <CiSquarePlus />}
        </IconButton>
        <FormCheckbox
          checked={children.every((c) => selectedFiles.has(c))}
          label={`${
            tree.name ? tree.name + ", " : ""
          } ${getTotalSelectedFiles()} files, ${formatBytes(
            getTotalSelectedBytes(),
          )}`}
          name={tree.id}
          onChange={handleToggleTree}
        ></FormCheckbox>
      </div>

      <div className="pl-5" hidden={!expanded}>
        {tree.dirs.map((dir) => (
          <FileTreeComponent
            torrentDetails={torrentDetails}
            torrentStats={torrentStats}
            key={dir.name}
            tree={dir}
            selectedFiles={selectedFiles}
            setSelectedFiles={setSelectedFiles}
            initialExpanded={false}
            showProgressBar={showProgressBar}
            disabled={disabled}
          />
        ))}
        <div className="pl-1">
          {tree.files.map((file) => (
            <div
              key={file.id}
              className={`${
                showProgressBar
                  ? "grid grid-cols-1 gap-1 items-start lg:grid-cols-2 mb-2 lg:mb-0"
                  : ""
              }`}
            >
              <FormCheckbox
                checked={selectedFiles.has(file.id)}
                label={`${file.filename} (${formatBytes(file.length)})`}
                name={`file-${file.id}`}
                disabled={disabled}
                onChange={() => handleToggleFile(file.id)}
              ></FormCheckbox>
              {showProgressBar && (
                <ProgressBar
                  now={(file.have_bytes / file.length) * 100}
                  variant={file.have_bytes == file.length ? "success" : "info"}
                />
              )}
            </div>
          ))}
        </div>
      </div>
    </>
  );
};

export const FileListInput: React.FC<{
  torrentDetails: TorrentDetails;
  torrentStats: TorrentStats | null;
  selectedFiles: Set<number>;
  setSelectedFiles: (_: Set<number>) => void;
  showProgressBar?: boolean;
  disabled?: boolean;
}> = ({
  torrentDetails,
  selectedFiles,
  setSelectedFiles,
  torrentStats,
  showProgressBar,
  disabled,
}) => {
  let fileTree = useMemo(
    () => newFileTree(torrentDetails, torrentStats),
    [torrentDetails, torrentStats],
  );

  return (
    <FileTreeComponent
      torrentDetails={torrentDetails}
      torrentStats={torrentStats}
      tree={fileTree}
      selectedFiles={selectedFiles}
      setSelectedFiles={setSelectedFiles}
      initialExpanded={true}
      showProgressBar={showProgressBar}
      disabled={disabled}
    />
  );
};
