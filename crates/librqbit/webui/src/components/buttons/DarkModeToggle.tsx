import { useState, useEffect } from "react";

const DarkModeToggle = () => {
  const getInitialState = () => {
    const storedDarkMode = localStorage.getItem("darkMode");
    return storedDarkMode ? JSON.parse(storedDarkMode) : false;
  };
  const [isDarkMode, setIsDarkMode] = useState<boolean>(getInitialState);

  useEffect(() => {
    document.body.classList.toggle("dark", isDarkMode);
    localStorage.setItem("darkMode", JSON.stringify(isDarkMode));
  }, [isDarkMode]);

  return (
    <div className="fixed bottom-5 right-5">
      <button
        onClick={() => setIsDarkMode((prev) => !prev)}
        className="ml-2 text-gray-700 dark:text-gray-300"
      >
        Dark Mode
      </button>
    </div>
  );
};

export default DarkModeToggle;
