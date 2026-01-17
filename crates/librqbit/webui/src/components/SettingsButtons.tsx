import { JSX, useState } from "react";
import { BsBodyText, BsMoon, BsSliders2, BsSun } from "react-icons/bs";
import { IconButton } from "./buttons/IconButton";
import { ViewModeToggle } from "./ViewModeToggle";
import { DarkMode } from "../helper/darkMode";
import { ConfigModal } from "./config/ConfigModal";

interface SettingsButtonsProps {
  onLogsClick: () => void;
  menuButtons?: JSX.Element[];
}

export const SettingsButtons: React.FC<SettingsButtonsProps> = ({
  onLogsClick,
  menuButtons,
}) => {
  const [isDark, setIsDark] = useState(DarkMode.isDark());
  const [configOpen, setConfigOpen] = useState(false);

  const handleDarkModeToggle = () => {
    DarkMode.toggle();
    setIsDark(DarkMode.isDark());
  };

  // Hide built-in configure button when custom menuButtons are provided
  // (e.g., desktop provides its own full config button)
  const showBuiltInConfigButton = !menuButtons || menuButtons.length === 0;

  return (
    <div className="flex items-center gap-1">
      {menuButtons?.map((b, i) => (
        <span key={i}>{b}</span>
      ))}
      {showBuiltInConfigButton && (
        <>
          <IconButton onClick={() => setConfigOpen(true)} title="Configure">
            <BsSliders2 />
          </IconButton>
          <ConfigModal
            isOpen={configOpen}
            onClose={() => setConfigOpen(false)}
          />
        </>
      )}
      <IconButton onClick={onLogsClick} title="View logs">
        <BsBodyText />
      </IconButton>
      <IconButton onClick={handleDarkModeToggle} title="Toggle dark mode">
        {isDark ? <BsSun /> : <BsMoon />}
      </IconButton>
      <ViewModeToggle />
    </div>
  );
};
