/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      // The eight tokens are CSS variables (see src/lib/theme.ts), so a
      // theme switch is one style update on <html>. The `rgb(... /
      // <alpha-value>)` form keeps every existing `/40`-style opacity
      // class working.
      colors: {
        graphite: "rgb(var(--c-bg) / <alpha-value>)",
        panel: "rgb(var(--c-panel) / <alpha-value>)",
        line: "rgb(var(--c-line) / <alpha-value>)",
        ink: "rgb(var(--c-ink) / <alpha-value>)",
        muted: "rgb(var(--c-muted) / <alpha-value>)",
        amber: "rgb(var(--c-accent) / <alpha-value>)",
        teal: "rgb(var(--c-ok) / <alpha-value>)",
        rose: "rgb(var(--c-danger) / <alpha-value>)",
        onaccent: "rgb(var(--c-on-accent) / <alpha-value>)",
      },
      fontFamily: {
        // System-native stacks on purpose, not a web-font import: a
        // local-first engine that phones home to Google Fonts just to
        // render its own control panel would undercut the product's
        // core claim. Persian gets its own stack so the glyphs look
        // right instead of falling back to a random sans.
        display: [
          "Söhne",
          "ui-sans-serif",
          "-apple-system",
          "Segoe UI",
          "Inter",
          "sans-serif",
        ],
        mono: [
          "ui-monospace",
          "JetBrains Mono",
          "Fira Code",
          "SFMono-Regular",
          "Menlo",
          "monospace",
        ],
        farsi: ["Vazirmatn", "Tahoma", "Segoe UI", "sans-serif"],
      },
      boxShadow: {
        node: "0 0 0 3px rgb(var(--c-accent) / 0.15)",
      },
    },
  },
  plugins: [],
};
