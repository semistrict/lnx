/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      fontFamily: {
        mono: ['"IBM Plex Mono"', '"SFMono-Regular"', "Consolas", "monospace"],
        sans: ['"IBM Plex Sans"', '"Avenir Next"', "ui-sans-serif", "system-ui"],
      },
      boxShadow: {
        terminal: "0 24px 80px rgba(0,0,0,0.38)",
      },
    },
  },
  plugins: [],
};
