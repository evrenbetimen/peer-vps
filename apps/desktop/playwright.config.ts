import { defineConfig, devices } from "@playwright/test";

// QA suite: drives the real UI bundle (vite preview) against the browser mock bridge.
// Chromium matches the Linux/Windows webviews; WebKit matches macOS's WKWebView.
export default defineConfig({
  testDir: "e2e",
  outputDir: "test-results",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: [["list"], ["html", { open: "never", outputFolder: "playwright-report" }]],
  use: {
    baseURL: "http://localhost:4173",
    viewport: { width: 1360, height: 860 },
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"], viewport: { width: 1360, height: 860 } } },
    { name: "webkit", use: { ...devices["Desktop Safari"], viewport: { width: 1360, height: 860 } } },
  ],
  webServer: {
    command: "pnpm exec vite preview --port 4173 --strictPort",
    url: "http://localhost:4173",
    reuseExistingServer: !process.env.CI,
  },
});
