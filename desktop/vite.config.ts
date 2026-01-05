import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5176,
    strictPort: true,
  },
  build: {
    target: "es2022",
    outDir: "dist",
  },
})
