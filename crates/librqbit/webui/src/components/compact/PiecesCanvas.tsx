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
        setBitmap(new Uint8Array(buffer));
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
  }, [torrentId, totalPieces, stats?.state, stats?.finished, API]);

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

    // Colors
    const haveColor = "#22c55e"; // green-500
    const partialColor = "#86efac"; // green-300 for partial
    const missingColor = "#374151"; // gray-700

    // Fill background with missing color
    ctx.fillStyle = missingColor;
    ctx.fillRect(0, 0, canvasWidth, CANVAS_HEIGHT);

    // Helper to check if piece is present
    const hasPiece = (pieceIndex: number): boolean => {
      const byteIndex = Math.floor(pieceIndex / 8);
      const bitIndex = 7 - (pieceIndex % 8); // MSB0 ordering
      return (
        byteIndex < bitmap.length &&
        ((bitmap[byteIndex] >> bitIndex) & 1) === 1
      );
    };

    if (totalPieces <= canvasWidth) {
      // Few pieces: each piece gets a rectangle
      const pieceWidth = canvasWidth / totalPieces;

      for (let i = 0; i < totalPieces; i++) {
        if (hasPiece(i)) {
          const x = Math.floor(i * pieceWidth);
          const nextX = Math.floor((i + 1) * pieceWidth);
          const width = nextX - x;
          ctx.fillStyle = haveColor;
          ctx.fillRect(x, 0, width, CANVAS_HEIGHT);
        }
      }
    } else {
      // Many pieces: aggregate multiple pieces per pixel column
      const piecesPerPixel = totalPieces / canvasWidth;

      for (let x = 0; x < canvasWidth; x++) {
        const startPiece = Math.floor(x * piecesPerPixel);
        const endPiece = Math.min(
          Math.ceil((x + 1) * piecesPerPixel),
          totalPieces
        );

        // Count how many pieces in this column we have
        let haveCount = 0;
        const totalCount = endPiece - startPiece;

        for (let p = startPiece; p < endPiece; p++) {
          if (hasPiece(p)) {
            haveCount++;
          }
        }

        // Color based on completion ratio
        if (haveCount === totalCount) {
          ctx.fillStyle = haveColor;
          ctx.fillRect(x, 0, 1, CANVAS_HEIGHT);
        } else if (haveCount > 0) {
          ctx.fillStyle = partialColor;
          ctx.fillRect(x, 0, 1, CANVAS_HEIGHT);
        }
        // Missing pieces keep the background color
      }
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
        style={{ height: `${CANVAS_HEIGHT}px` }}
      />
    </div>
  );
};
