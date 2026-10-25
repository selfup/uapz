import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    watch: {
      ignored: [
        "**/src/**",
        "**/src-tauri/**",
        "**/target/**",
        "**/runs/**",
        "**/cache/**",
      ],
    },
  },
  test: { environment: "node", include: ["frontend/**/*.test.{ts,tsx}"] },
});
