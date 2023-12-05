import type { Config } from "tailwindcss";
import { createThemes } from "tw-colors";

const config: Config = {
  content: [
    "./src/pages/**/*.{js,ts,jsx,tsx,mdx}",
    "./src/components/**/*.{js,ts,jsx,tsx,mdx}",
    "./src/common/**/*.{js,ts,jsx,tsx,mdx}",
    "./src/header/**/*.{js,ts,jsx,tsx,mdx}",
    "./src/app/**/*.{js,ts,jsx,tsx,mdx}",
  ],
  theme: {
    extend: {
      backgroundImage: {
        "gradient-radial": "radial-gradient(var(--tw-gradient-stops))",
        "gradient-conic":
          "conic-gradient(from 180deg at 50% 50%, var(--tw-gradient-stops))",
      },
    },
  },
  plugins: [
    createThemes({
      light: {
        primary: "steelblue",
        secondary: "darkblue",
        brand: "#F3F3F3",
      },
      dark: {
        primary: "turquoise",
        secondary: "tomato",
        brand: "#4A4A4A",
      },
      forest: {
        primary: "#2A9D8F",
        secondary: "#E9C46A",
        brand: "#264653",
      },
      winter: {
        primary: "hsl(45 39% 69%)",
        secondary: "rgb(120 210 50)",
        brand: "hsl(54 93% 96%)",
      },
    }),
  ],
};
export default config;
