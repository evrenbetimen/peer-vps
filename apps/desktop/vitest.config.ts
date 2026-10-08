import { defineConfig, mergeConfig } from "vitest/config";

import viteConfig from "./vite.config";

export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      environment: "jsdom",
      include: ["src/**/*.test.{ts,tsx}"],
      setupFiles: ["src/test/setup.ts"],
      restoreMocks: true,
      coverage: {
        provider: "v8",
        include: ["src/**/*.{ts,tsx}"],
        exclude: ["src/**/*.test.{ts,tsx}", "src/test/**", "src/main.tsx", "src/components/Terminal.tsx"],
        reporter: ["text-summary", "html"],
        // QC gate: CI fails if coverage of the UI logic drops below these.
        thresholds: { lines: 70, functions: 65, branches: 60, statements: 70 },
      },
    },
  }),
);
