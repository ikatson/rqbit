import { useState, useEffect } from "react";

const LARGE_SCREEN_BREAKPOINT = 1024;

export function useIsLargeScreen(): boolean {
  const [isLarge, setIsLarge] = useState(
    () => window.innerWidth >= LARGE_SCREEN_BREAKPOINT,
  );

  useEffect(() => {
    const handleResize = () => {
      setIsLarge(window.innerWidth >= LARGE_SCREEN_BREAKPOINT);
    };

    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, []);

  return isLarge;
}
