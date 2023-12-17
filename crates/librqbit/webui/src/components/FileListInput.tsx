import { useMemo, useState } from "react";
import { AddTorrentResponse } from "../api-types";
import { FormCheckbox } from "./forms/FormCheckbox";
import { CiSquarePlus, CiSquareMinus } from "react-icons/ci";
import { IconButton } from "./buttons/IconButton";
import { formatBytes } from "../helper/formatBytes";

type TorrentFileForCheckbox = {
  id: number;
  name: string;
  length: number;
};

type FileTree = {
  id: string;
  name: string;
  dirs: FileTree[];
  files: TorrentFileForCheckbox[];
};

const splitOnce = (s: string, sep: string): [string, string | undefined] => {
  if (s.indexOf(sep) === -1) {
    return [s, undefined];
  }
  return [s.slice(0, s.indexOf(sep)), s.slice(s.indexOf(sep) + 1)];
};

const newFileTree = (listTorrentResponse: AddTorrentResponse): FileTree => {
  const separator = "/";
  const newFileTreeInner = (
    name: string,
    id: string,
    files: TorrentFileForCheckbox[]
  ): FileTree => {
    let directFiles: TorrentFileForCheckbox[] = [];
    let groups: FileTree[] = [];
    let groupsByName: { [key: string]: TorrentFileForCheckbox[] } = {};

    const getGroup = (prefix: string): TorrentFileForCheckbox[] => {
      groupsByName[prefix] = groupsByName[prefix] || [];
      return groupsByName[prefix];
    };

    files.forEach((file) => {
      let [prefix, name] = splitOnce(file.name, separator);
      if (name === undefined) {
        directFiles.push(file);
        return;
      }
      getGroup(prefix).push({
        id: file.id,
        name: name,
        length: file.length,
      });
    });

    let childId = 0;
    for (const [key, value] of Object.entries(groupsByName)) {
      groups.push(newFileTreeInner(key, id + "." + childId, value));
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
    listTorrentResponse.details.files.map((data, id) => {
      return { id, name: data.name, length: data.length };
    })
  );
};

const FileTreeComponent: React.FC<{
  tree: FileTree;
  listTorrentResponse: AddTorrentResponse;
  selectedFiles: Set<number>;
  setSelectedFiles: React.Dispatch<React.SetStateAction<Set<number>>>;
  initialExpanded: boolean;
}> = ({
  tree,
  selectedFiles,
  setSelectedFiles,
  initialExpanded,
  listTorrentResponse,
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
      .map((c) => listTorrentResponse.details.files[c].length)
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
            getTotalSelectedBytes()
          )}`}
          name={tree.id}
          onChange={handleToggleTree}
        ></FormCheckbox>
      </div>

      <div className="pl-5" hidden={!expanded}>
        {tree.dirs.map((dir) => (
          <FileTreeComponent
            listTorrentResponse={listTorrentResponse}
            key={dir.name}
            tree={dir}
            selectedFiles={selectedFiles}
            setSelectedFiles={setSelectedFiles}
            initialExpanded={false}
          />
        ))}
        <div className="pl-1">
          {tree.files.map((file) => (
            <FormCheckbox
              checked={selectedFiles.has(file.id)}
              key={file.id}
              label={`${file.name} (${formatBytes(file.length)})`}
              name={`file-${file.id}`}
              onChange={() => handleToggleFile(file.id)}
            ></FormCheckbox>
          ))}
        </div>
      </div>
    </>
  );
};

export const FileListInput: React.FC<{
  listTorrentResponse: AddTorrentResponse;
  selectedFiles: Set<number>;
  setSelectedFiles: React.Dispatch<React.SetStateAction<Set<number>>>;
}> = ({ listTorrentResponse, selectedFiles, setSelectedFiles }) => {
  let fileTree = useMemo(
    () => newFileTree(listTorrentResponse),
    [listTorrentResponse]
  );

  return (
    <>
      <FileTreeComponent
        listTorrentResponse={listTorrentResponse}
        tree={fileTree}
        selectedFiles={selectedFiles}
        setSelectedFiles={setSelectedFiles}
        initialExpanded={true}
      />
    </>
  );
};
