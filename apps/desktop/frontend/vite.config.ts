import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// SOLEXA frontend, a thin client of the Rust coordinator (see the
// architecture note in src/lib/api.ts). Standard Tauri-flavoured Vite
// config: fixed dev port so tauri.conf.json's devUrl matches, and we
// don't clear the terminal so Cargo's own build output stays visible
// alongside Vite's when running through `cargo tauri dev`.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "esnext",
    outDir: "dist",
  },
});
