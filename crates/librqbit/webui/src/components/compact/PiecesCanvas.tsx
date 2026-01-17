import { useRef, useEffect, useContext, useState } from "react";
import { APIContext } from "../../context";
import { customSetInterval } from "../../helper/customSetInterval";
import { STATE_LIVE, STATE_INITIALIZING, TorrentStats } from "../../api-types";

interface PiecesCanvasProps {
  torrentId: number;
  totalPieces: number;
  stats: TorrentStats | null;
}

const CANVAS_HEIGHT = 12;

export const PiecesCanvas: React.FC<PiecesCanvasProps> = ({
  torrentId,
  totalPieces,
  stats,
}) => {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const API = useContext(APIContext);
  const [bitmap, setBitmap] = useState<Uint8Array | null>(null);

  // Fetch bitmap with adaptive polling
  useEffect(() => {
    if (totalPieces === 0) return;

    return customSetInterval(async () => {
      try {
        const buffer = await API.getTorrentHaves(torrentId);
        setBitmap(buffer);
      } catch (e) {
        console.error("Failed to fetch haves:", e);
      }

      // Poll faster when downloading, slower when paused/seeding
      const isActive =
        stats?.state === STATE_LIVE || stats?.state === STATE_INITIALIZING;
      const isFinished = stats?.finished ?? false;

      if (isActive && !isFinished) {
        return 2000; // 2s while downloading
      }
      return 30000; // 30s when paused or seeding
    }, 0);
  }, [torrentId, totalPieces, stats?.state, stats?.finished]);

  // Render bitmap to canvas - single horizontal line
  useEffect(() => {
    const canvas = canvasRef.current;
    const container = containerRef.current;
    if (!canvas || !container || !bitmap || totalPieces === 0) return;

    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const containerWidth = container.clientWidth;
    const canvasWidth = Math.max(containerWidth, 100);

    canvas.width = canvasWidth;
    canvas.height = CANVAS_HEIGHT;

    // Get colors from CSS custom properties (read from body where .dark class is applied)
    const computedStyle = getComputedStyle(document.body);
    const haveColor =
      computedStyle.getPropertyValue("--color-success-bg").trim() || "#22c55e";
    const missingColor =
      computedStyle.getPropertyValue("--color-divider").trim() || "#374151";

    // Fill background with missing color
    ctx.fillStyle = missingColor;
    ctx.fillRect(0, 0, canvasWidth, CANVAS_HEIGHT);

    // Helper to check if piece is present
    const hasPiece = (pieceIndex: number): boolean => {
      const byteIndex = Math.floor(pieceIndex / 8);
      const bitIndex = 7 - (pieceIndex % 8); // MSB0 ordering
      return (
        byteIndex < bitmap.length && ((bitmap[byteIndex] >> bitIndex) & 1) === 1
      );
    };

    const pieceWidth = canvasWidth / totalPieces;
    ctx.fillStyle = haveColor;

    let runStart = -1;

    for (let i = 0; i < totalPieces; i++) {
      if (hasPiece(i)) {
        if (runStart === -1) runStart = i;
      } else {
        if (runStart !== -1) {
          const x = runStart * pieceWidth;
          const width = (i - runStart) * pieceWidth;
          ctx.fillRect(x, 0, width, CANVAS_HEIGHT);
          runStart = -1;
        }
      }
    }

    if (runStart !== -1) {
      const x = runStart * pieceWidth;
      const width = (totalPieces - runStart) * pieceWidth;
      ctx.fillRect(x, 0, width, CANVAS_HEIGHT);
    }
  }, [bitmap, totalPieces]);

  if (totalPieces === 0) {
    return null;
  }

  return (
    <div ref={containerRef} className="w-full">
      <canvas
        ref={canvasRef}
        className="w-full rounded"
        style={{ height: `${CANVAS_HEIGHT}px`, imageRendering: "pixelated" }}
      />
    </div>
  );
};
