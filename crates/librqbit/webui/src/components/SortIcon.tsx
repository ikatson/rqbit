import { GoChevronDown, GoChevronUp } from "react-icons/go";
import { JSX } from "react";

type SortDirection = "asc" | "desc";

interface SortIconProps<T extends string> {
  column: T;
  sortColumn: T;
  sortDirection: SortDirection;
}

export function SortIcon<T extends string>({
  column,
  sortColumn,
  sortDirection,
}: SortIconProps<T>): JSX.Element | null {
  if (column !== sortColumn) {
    return null;
  }
  return sortDirection === "asc" ? (
    <GoChevronUp className="inline ml-0.5 w-4 h-4" />
  ) : (
    <GoChevronDown className="inline ml-0.5 w-4 h-4" />
  );
}
