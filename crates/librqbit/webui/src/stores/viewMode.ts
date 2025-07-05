import { createContext } from "react";

export const ViewModeContext = createContext({
  compact: false,
  toggleCompact: () => {},
});
