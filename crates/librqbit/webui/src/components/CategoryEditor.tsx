import { useContext, useEffect, useState, useMemo } from "react";
import { APIContext } from "../context";
import { useTorrentStore } from "../stores/torrentStore";
import { useErrorStore } from "../stores/errorStore";
import { ErrorDetails } from "../api-types";

// Inline editor for a torrent's category, usable after the torrent was created.
// Uses a dropdown populated from the server's configured categories.
export const CategoryEditor: React.FC<{
  torrentId: number;
  category: string | null | undefined;
}> = ({ torrentId, category }) => {
  const API = useContext(APIContext);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const setCloseableError = useErrorStore((state) => state.setCloseableError);

  const [saving, setSaving] = useState(false);
  const [categories, setCategories] = useState<string[]>([]);

  useEffect(() => {
    API.listCategories()
      .then((cats) => setCategories(cats))
      .catch(() => setCategories([]));
  }, [API]);

  const save = async (newCategory: string) => {
    setSaving(true);
    try {
      await API.updateCategory(torrentId, newCategory || null);
      refreshTorrents();
    } catch (e) {
      setCloseableError({
        text: "Error updating category",
        details: e as ErrorDetails,
      });
    } finally {
      setSaving(false);
    }
  };

  return (
    <span className="inline-flex items-center gap-2">
      <select
        value={category ?? ""}
        onChange={(e) => save(e.target.value)}
        disabled={saving}
        className="border border-divider rounded bg-transparent px-1 py-0.5 text-sm focus:outline-none focus:border-primary disabled:opacity-50"
      >
        <option value="">None</option>
        {categories.map((cat) => (
          <option key={cat} value={cat}>
            {cat}
          </option>
        ))}
      </select>
    </span>
  );
};
