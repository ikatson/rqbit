import { SortIcon } from "../SortIcon";
import { TableSortColumn } from "./TorrentTable";
import { SortDirection } from "../../helper/torrentFilters";

interface TableHeaderProps {
  column: TableSortColumn;
  label: string;
  sortColumn: TableSortColumn;
  sortDirection: SortDirection;
  onSort: (column: TableSortColumn) => void;
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
