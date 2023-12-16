let darkMode = window.matchMedia("(prefers-color-scheme: dark)").matches;

window
  .matchMedia("(prefers-color-scheme: dark)")
  .addEventListener("change", (event) => {
    DarkMode.setDark(event.matches);
  });

export const DarkMode = {
  isDark: () => darkMode,
  setDark: (value: boolean) => {
    darkMode = value;
    document.body.classList.toggle("dark", darkMode);
    return darkMode;
  },
  toggle: () => {
    DarkMode.setDark(!darkMode);
  },
};

DarkMode.setDark(darkMode);
