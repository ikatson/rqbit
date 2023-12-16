import rqbitWebTailwind from "../crates/librqbit/webui/tailwind.config";

import type { Config } from "tailwindcss";

export default {
  ...rqbitWebTailwind,
  content: [
    "./src/**/*.{js,ts,jsx,tsx,mdx}",
    "../crates/librqbit/webui/src/**/*.{js,ts,jsx,tsx,mdx}",
  ],
} satisfies Config;
