import { useState, useCallback, useEffect, useRef } from "react";
import { TorrentListItem } from "../../api-types";
import { ActionBar } from "./ActionBar";
import { TorrentTable } from "./TorrentTable";
import { DetailPane } from "./DetailPane";

const DETAIL_PANE_MIN_HEIGHT = 100;
const DETAIL_PANE_MAX_HEIGHT = 600;
const DETAIL_PANE_DEFAULT_HEIGHT = 256;

interface CompactLayoutProps {
  torrents: TorrentListItem[] | null;
  loading: boolean;
}

export const CompactLayout: React.FC<CompactLayoutProps> = ({
  torrents,
  loading,
}) => {
  const [detailPaneHeight, setDetailPaneHeight] = useState(
    DETAIL_PANE_DEFAULT_HEIGHT,
  );
  const [isDragging, setIsDragging] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    setIsDragging(true);
  }, []);

  useEffect(() => {
    if (!isDragging) return;

    const handleMouseMove = (e: MouseEvent) => {
      if (!containerRef.current) return;

      const containerRect = containerRef.current.getBoundingClientRect();
      const newHeight = containerRect.bottom - e.clientY;
      const clampedHeight = Math.max(
        DETAIL_PANE_MIN_HEIGHT,
        Math.min(DETAIL_PANE_MAX_HEIGHT, newHeight),
      );
      setDetailPaneHeight(clampedHeight);
    };

    const handleMouseUp = () => {
      setIsDragging(false);
    };

    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);

    return () => {
      document.removeEventListener("mousemove", handleMouseMove);
      document.removeEventListener("mouseup", handleMouseUp);
    };
  }, [isDragging]);

  const hasTorrents = (torrents?.length ?? 0) > 0;

  return (
    <div ref={containerRef} className="flex flex-col h-full">
      {hasTorrents && <ActionBar />}
      <div className="flex-1 overflow-auto min-h-0">
        <TorrentTable torrents={torrents} loading={loading} />
      </div>
      {hasTorrents && (
        <>
          {/* Draggable divider */}
          <div
            onMouseDown={handleMouseDown}
            className={`h-1.5 cursor-ns-resize shrink-0 bg-divider hover:bg-primary transition-colors ${isDragging ? "bg-primary" : ""}`}
            title="Drag to resize"
          >
            <div className="h-full w-12 mx-auto flex items-center justify-center">
              <div className="w-8 h-0.5 bg-secondary rounded-full" />
            </div>
          </div>
          <div style={{ height: detailPaneHeight }} className="shrink-0">
            <DetailPane />
          </div>
        </>
      )}
    </div>
  );
};
