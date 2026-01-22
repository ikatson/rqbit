import { useEffect } from "react";
import { useUIStore } from "../stores/uiStore";
import { useTorrentStore } from "../stores/torrentStore";
import { isTorrentVisible } from "../helper/torrentFilters";

interface KeyboardShortcutActions {
  onDelete?: () => void;
}

/**
 * Hook that sets up keyboard shortcuts for the compact view.
 * Should be called in ActionBar or CompactLayout.
 */
export function useKeyboardShortcuts(actions?: KeyboardShortcutActions) {
  const torrents = useTorrentStore((state) => state.torrents);
  const searchQuery = useUIStore((state) => state.searchQuery);
  const statusFilter = useUIStore((state) => state.statusFilter);
  const selectAll = useUIStore((state) => state.selectAll);
  const clearSelection = useUIStore((state) => state.clearSelection);
  const selectedTorrentIds = useUIStore((state) => state.selectedTorrentIds);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Ignore if user is typing in an input field
      const target = e.target as HTMLElement;
      if (
        target.tagName === "INPUT" ||
        target.tagName === "TEXTAREA" ||
        target.isContentEditable
      ) {
        return;
      }

      // Ignore if a modal is open (modals have role="dialog")
      if (document.querySelector('[role="dialog"]')) {
        return;
      }

      const isMod = e.metaKey || e.ctrlKey;

      // Ctrl/Cmd+A: Select all visible torrents
      if (isMod && e.key === "a") {
        e.preventDefault();
        if (torrents) {
          const normalizedQuery = searchQuery.toLowerCase();
          const visibleIds = torrents
            .filter((t) => isTorrentVisible(t, normalizedQuery, statusFilter))
            .map((t) => t.id);
          selectAll(visibleIds);
        }
        return;
      }

      // Ctrl/Cmd+F: Focus search input
      if (isMod && e.key === "f") {
        e.preventDefault();
        const searchInput = document.querySelector<HTMLInputElement>(
          "[data-search-input]",
        );
        searchInput?.focus();
        searchInput?.select();
        return;
      }

      // Escape: Clear selection
      if (e.key === "Escape") {
        e.preventDefault();
        clearSelection();
        return;
      }

      // Delete/Backspace: Open delete modal for selected torrents
      if (
        (e.key === "Delete" || e.key === "Backspace") &&
        selectedTorrentIds.size > 0
      ) {
        e.preventDefault();
        actions?.onDelete?.();
        return;
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [
    torrents,
    searchQuery,
    statusFilter,
    selectAll,
    clearSelection,
    selectedTorrentIds,
    actions,
  ]);
}
