import { JSX, useState } from "react";
import { BsBodyText, BsMoon, BsSun } from "react-icons/bs";
import { IconButton } from "./buttons/IconButton";
import { ViewModeToggle } from "./ViewModeToggle";
import { DarkMode } from "../helper/darkMode";

interface SettingsButtonsProps {
  onLogsClick: () => void;
  menuButtons?: JSX.Element[];
}

export const SettingsButtons: React.FC<SettingsButtonsProps> = ({
  onLogsClick,
  menuButtons,
}) => {
  const [isDark, setIsDark] = useState(DarkMode.isDark());

  const handleDarkModeToggle = () => {
    DarkMode.toggle();
    setIsDark(DarkMode.isDark());
  };

  return (
    <div className="flex items-center gap-1">
      {menuButtons?.map((b, i) => <span key={i}>{b}</span>)}
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
