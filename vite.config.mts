import react from "@vitejs/plugin-react";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const root = path.dirname(fileURLToPath(import.meta.url));
const renderer = path.join(root, "src", "renderer");
const aliases = [
    { find: "renderer", replacement: renderer },
    { find: "shared", replacement: path.join(root, "src", "shared") },
    { find: "main", replacement: path.join(root, "src", "main") },
    { find: "assets", replacement: path.join(root, "assets") },
];

export default defineConfig({
    root: renderer,
    base: "./",
    resolve: { alias: aliases },
    plugins: [react()],
    server: { host: "127.0.0.1", port: 1420, strictPort: true },
    build: {
        outDir: path.join(root, "release", "tauri", "ui"),
        emptyOutDir: true,
        rollupOptions: {
            input: [
                "index.html",
                "launcher.html",
                "oneclick-download-map.html",
                "oneclick-download-playlist.html",
                "oneclick-download-model.html",
                "shortcut-launch.html",
            ].map(page => path.join(renderer, page)),
        },
    },
});
