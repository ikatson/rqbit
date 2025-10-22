import React, { useState, useRef, useCallback, useEffect } from "react";

interface ResizablePanesProps {
  top: React.ReactNode;
  bottom: React.ReactNode;
  initialTopHeight?: string;
  minTopHeight?: number;
  minBottomHeight?: number;
}

export const ResizablePanes: React.FC<ResizablePanesProps> = ({
  top,
  bottom,
  initialTopHeight = "50%",
  minTopHeight = 50,
  minBottomHeight = 50,
}) => {
  const [topHeight, setTopHeight] = useState(initialTopHeight);
  const containerRef = useRef<HTMLDivElement>(null);

  const onMouseMove = useCallback(
    (e: MouseEvent) => {
      if (containerRef.current) {
        const containerRect = containerRef.current.getBoundingClientRect();
        let newTopHeight = e.clientY - containerRect.top;

        if (newTopHeight < minTopHeight) {
          newTopHeight = minTopHeight;
        }
        if (newTopHeight > containerRect.height - minBottomHeight) {
          newTopHeight = containerRect.height - minBottomHeight;
        }

        setTopHeight(`${newTopHeight}px`);
      }
    },
    [minTopHeight, minBottomHeight],
  );

  const onMouseUp = useCallback(() => {
    document.removeEventListener("mousemove", onMouseMove);
    document.removeEventListener("mouseup", onMouseUp);
  }, [onMouseMove]);

  const onMouseDown = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      document.addEventListener("mousemove", onMouseMove);
      document.addEventListener("mouseup", onMouseUp);
    },
    [onMouseMove, onMouseUp],
  );

  return (
    <div ref={containerRef} className="flex flex-col h-full">
      <div style={{ height: topHeight }} className="overflow-y-auto">
        {top}
      </div>
      <div
        className="h-2 bg-gray-300 dark:bg-gray-700 cursor-row-resize hover:bg-blue-500"
        onMouseDown={onMouseDown}
        role="separator"
        aria-label="Resize panes"
      />
      <div className="flex-grow overflow-y-auto flex flex-col">{bottom}</div>
    </div>
  );
};
