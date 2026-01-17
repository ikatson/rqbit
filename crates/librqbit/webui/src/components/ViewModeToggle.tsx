import { BsGrid, BsListUl } from "react-icons/bs";
import { useUIStore } from "../stores/uiStore";
import { useIsLargeScreen } from "../hooks/useIsLargeScreen";
import { IconButton } from "./buttons/IconButton";

export const ViewModeToggle: React.FC = () => {
  const viewMode = useUIStore((state) => state.viewMode);
  const toggleViewMode = useUIStore((state) => state.toggleViewMode);
  const isLargeScreen = useIsLargeScreen();

  if (!isLargeScreen) {
    return null;
  }

  const tooltipText =
    viewMode === "compact" ? "Switch to card view" : "Switch to table view";

  return (
    <IconButton onClick={toggleViewMode} title={tooltipText}>
      {viewMode === "compact" ? <BsGrid /> : <BsListUl />}
    </IconButton>
  );
};
