import { defineConfig } from "vite";

export default defineConfig(({ mode }) => {
  const native = mode === "native";
  return {
    build: {
      target: "es2022",
      outDir: native ? "../../../src/web/assets" : "dist",
      emptyOutDir: true,
      sourcemap: !native,
      ...(native ? {
        rollupOptions: {
          output: {
            entryFileNames: "app.js",
            assetFileNames: (asset) => asset.names.some((name) => name.endsWith(".css")) ? "app.css" : "[name][extname]",
          },
        },
      } : {}),
    },
  };
});
