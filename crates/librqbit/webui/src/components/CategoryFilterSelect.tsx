import { useContext, useEffect, useState } from "react";
import { APIContext } from "../context";
import { useUIStore } from "../stores/uiStore";
import {
  CATEGORY_FILTER_ALL,
  CATEGORY_FILTER_NONE,
} from "../helper/torrentFilters";

// A dropdown to filter the torrent list by category.
// Populated from the server's configured categories, with "All" and "Uncategorized" options.
// Filtering itself is done client-side via isTorrentVisible.
export const CategoryFilterSelect: React.FC<{ className?: string }> = ({
  className,
}) => {
  const API = useContext(APIContext);
  const categoryFilter = useUIStore((state) => state.categoryFilter);
  const setCategoryFilter = useUIStore((state) => state.setCategoryFilter);
  const [categories, setCategories] = useState<string[]>([]);

  useEffect(() => {
    API.listCategories()
      .then((cats) => setCategories(cats))
      .catch(() => setCategories([]));
  }, [API]);

  return (
    <select
      value={categoryFilter}
      onChange={(e) => setCategoryFilter(e.target.value)}
      title="Filter by category"
      className={className}
    >
      <option value={CATEGORY_FILTER_ALL}>All categories</option>
      <option value={CATEGORY_FILTER_NONE}>Uncategorized</option>
      {categories.map((c) => (
        <option key={c} value={c}>
          {c}
        </option>
      ))}
    </select>
  );
};
