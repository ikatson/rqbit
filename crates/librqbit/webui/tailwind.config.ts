import colors from "tailwindcss/colors";

import type { Config } from "tailwindcss";

export default {
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
  darkMode: "class",
  theme: {
    colors: {
      transparent: colors.transparent,
      amber: colors.amber,
      black: colors.black,
      blue: colors.blue,
      slate: colors.slate,
      white: colors.white,
      gray: colors.gray,
      green: colors.green,
      red: colors.red,
    },
  },
} satisfies Config;
