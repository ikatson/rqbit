import { SortIcon } from "../SortIcon";
import { TorrentSortColumn, SortDirection } from "./TorrentTable";

interface TableHeaderProps {
  column: TorrentSortColumn;
  label: string;
  sortColumn: TorrentSortColumn;
  sortDirection: SortDirection;
  onSort: (column: TorrentSortColumn) => void;
  className?: string;
  align?: "left" | "center" | "right";
}

export const TableHeader: React.FC<TableHeaderProps> = ({
  column,
  label,
  sortColumn,
  sortDirection,
  onSort,
  className = "",
  align = "left",
}) => {
  const alignClass =
    align === "center"
      ? "text-center"
      : align === "right"
        ? "text-right"
        : "text-left";

  return (
    <th
      className={`px-2 py-2 text-secondary cursor-pointer hover:text-text select-none whitespace-nowrap ${alignClass} ${className}`}
      onClick={() => onSort(column)}
    >
      {label}
      <SortIcon
        column={column}
        sortColumn={sortColumn}
        sortDirection={sortDirection}
      />
    </th>
  );
};
