import { expect, type Page, test } from "@playwright/test";

// Every scenario also asserts the console stayed clean: an uncaught error or a
// React warning is a QA failure even when the visible flow looks right.
test.beforeEach(async ({ page }) => {
  const problems: string[] = [];
  page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
  page.on("console", (m) => {
    if (m.type() === "error" || m.type() === "warning") problems.push(`${m.type()}: ${m.text()}`);
  });
  (page as Page & { problems?: string[] }).problems = problems;
  await page.goto("/");
  await expect(page.getByText("browser preview (mock data)")).toBeVisible();
});

test.afterEach(async ({ page }) => {
  expect((page as Page & { problems?: string[] }).problems ?? []).toEqual([]);
});

const nav = (page: Page, name: string) => page.getByRole("navigation").getByRole("button", { name: new RegExp(`^${name}`) });

async function shot(page: Page, name: string) {
  await page.screenshot({ path: `test-results/screenshots/${test.info().project.name}-${name}.png`, fullPage: true });
}

test("host dashboard streams live telemetry", async ({ page }) => {
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Provider mode");
  const cpu = page.getByText(/^CPU load · \d+ %$/);
  await expect(cpu).toBeVisible();
  const first = await cpu.textContent();
  await expect.poll(async () => cpu.textContent(), { timeout: 5000 }).not.toBe(first);
  await expect(page.getByText(/^Network · [\d.]+ [KMG]?B\/s$/)).toBeVisible();

  await page.getByRole("button", { name: /apply/i }).click();
  await expect(page.getByText(/Free: \d+ cores/)).toBeVisible();
  await shot(page, "host");
});

test("renter deploys, opens a shell, scales to zero and terminates", async ({ page }) => {
  await nav(page, "Console").click();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Renter & agents");
  await expect(page.getByText(/^3 · Offers · [1-9]/)).toBeVisible();
  await page.getByRole("button", { name: "Deploy" }).click();

  await expect(page.getByText(/^SSH · 10\.147\.0\.\d+$/)).toBeVisible();
  const term = page.locator(".xterm");
  await expect(term).toContainText("Overlay SSH transport not connected yet");
  await term.click();
  await page.keyboard.type("whoami");
  await page.keyboard.press("Enter");
  await expect(term).toContainText("whoami");
  await expect(page.getByText(/1 active/)).toBeVisible();
  await shot(page, "renter");

  await page.getByRole("button", { name: "Scale to 0" }).click();
  await expect(page.getByText("scaledToZero")).toBeVisible();
  await page.getByRole("button", { name: "Resume" }).click();
  await expect(page.getByRole("button", { name: "Scale to 0" })).toBeVisible();
  await page.getByRole("button", { name: "Terminate" }).click();
  await expect(page.getByText(/0 active/)).toBeVisible();
});

test("offer filters narrow the market", async ({ page }) => {
  await nav(page, "Console").click();
  await page.getByRole("button", { name: /Ubuntu \+ CUDA/ }).click();
  await page.getByRole("group", { name: "Max price" }).getByRole("button", { name: "500 cr/h" }).click();
  await page.getByRole("group", { name: "Min VRAM" }).getByRole("button", { name: "40 GiB" }).click();
  await expect(page.getByText("3 · Offers · 1")).toBeVisible();
  await expect(page.getByRole("cell", { name: /iad-h100-3/ })).toBeVisible();
  await page.getByRole("group", { name: "Max price" }).getByRole("button", { name: "5 cr/h", exact: true }).click();
  await expect(page.getByText("No offer matches.", { exact: false })).toBeVisible();
  await expect(page.getByRole("button", { name: "Deploy" })).toBeDisabled();
});

test("wallet top-up lands in the balance and the billing log", async ({ page }) => {
  await nav(page, "Wallet").click();
  const balance = page.getByText("Available credits").locator("..");
  await expect(balance).toContainText("cr");
  const before = parseFloat(((await balance.textContent()) ?? "").replace(/[^\d.]/g, ""));
  await page.getByRole("button", { name: "+25 cr" }).click();
  await expect(page.getByRole("cell", { name: "Top-up" }).first()).toBeVisible();
  await expect
    .poll(async () => parseFloat(((await balance.textContent()) ?? "").replace(/[^\d.]/g, "")))
    .toBeCloseTo(before + 25, 0);
  await shot(page, "wallet");
});

test("failover reroutes the virtual IP after three missed heartbeats and recovers", async ({ page }) => {
  await nav(page, "Failover").click();
  await expect(page.getByText("All peers healthy.", { exact: false })).toBeVisible();
  await page.getByRole("button", { name: "Kill Frankfurt" }).click();

  const log = page.getByRole("list").filter({ hasText: "host-b" });
  await expect(log).toContainText("suspect ×1");
  await expect(log).toContainText("rerouted → 10.147.0.200", { timeout: 5000 });
  await expect(page.locator("svg text", { hasText: "serving · healthy" })).toBeVisible();
  await shot(page, "failover");

  await page.getByRole("button", { name: "Restore Frankfurt" }).click();
  await expect(log).toContainText("recovered");
  await expect(page.getByRole("button", { name: "Kill Frankfurt" })).toBeVisible();
});

test("every view fits the minimum window without horizontal scroll", async ({ page }) => {
  await page.setViewportSize({ width: 1024, height: 680 }); // tauri.conf.json minWidth/minHeight
  for (const name of ["Host", "Console", "Wallet", "Failover"]) {
    await nav(page, name).click();
    const overflow = await page.locator("main").evaluate((el) => el.scrollWidth - el.clientWidth);
    expect(overflow, `${name} overflows by ${overflow}px`).toBeLessThanOrEqual(0);
  }
});

test("guest images can be downloaded from the host view", async ({ page }) => {
  const debian = page.getByTestId("image-debian-13");
  await expect(page.getByTestId("image-ubuntu-24.04")).toContainText("installed");
  await debian.getByRole("button", { name: "Download" }).click();
  await expect(debian).toContainText(/downloading/);
  await expect(debian).toContainText("installed", { timeout: 10_000 });
});

test("a deployed instance shows how to SSH in", async ({ page }) => {
  await nav(page, "Console").click();
  await expect(page.getByText(/^3 · Offers · [1-9]/)).toBeVisible();
  await page.getByRole("button", { name: "Deploy" }).click();
  await expect(page.getByRole("button", { name: /^ssh -p \d+ peervps@127\.0\.0\.1$/ })).toBeVisible();
  await expect(page.getByText("mock-password")).toBeVisible();
});

test("a Windows ISO is added and installs with a screen and Remote Desktop", async ({ page }) => {
  await page.getByRole("button", { name: "Add ISO or disk…" }).click();
  const iso = page.getByTestId("image-win11_24h2_english_arm64");
  await expect(iso).toContainText("installed", { timeout: 10_000 });
  await expect(iso).toContainText("Windows installer · ARM64");
  await nav(page, "Console").click();
  await page.getByRole("button", { name: /win11_24h2_english_arm64/ }).click({ timeout: 10_000 });
  await expect(page.getByText(/Windows signs in the user shown there/)).toBeVisible();
  await page.getByRole("button", { name: "Deploy" }).click();
  await expect(page.getByText("Remote Desktop", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Open screen" })).toBeVisible();
  await shot(page, "windows-iso");
});

test("peers: approve a machine that asked and rent another machine's offer", async ({ page }) => {
  await expect(page.getByText("pv-5c0ffee15ea1ab1e@192.168.1.10:7071")).toBeVisible();
  await page.getByRole("textbox", { name: "Peer address" }).fill("pv-0123456789abcdef@192.168.1.40:7071");
  await page.getByRole("button", { name: "Add peer" }).click();
  await expect(page.getByTestId("peer-pv-0123456789abcdef")).toContainText("waiting for their approval");
  await page.getByTestId("peer-pv-a17b0c55e9d24f13").getByRole("button", { name: "Approve" }).click();
  await expect(page.getByTestId("peer-pv-a17b0c55e9d24f13")).toContainText("online");
  await shot(page, "peers");
  await nav(page, "Console").click();
  await page.getByText("pv-3f9c1a7e2b4d6c80/this-machine").click();
  await page.getByRole("button", { name: "Deploy" }).click();
  await expect(page.getByText(/on pv-3f9c1a7e2b4d6c80/)).toBeVisible();
});

test("peers: a machine on the network is one click away and the router port can be opened", async ({ page }) => {
  await page.getByTestId("nearby-pv-b2e4f6a8c0d1e3f5").getByRole("button", { name: "Add" }).click();
  await expect(page.getByTestId("peer-pv-b2e4f6a8c0d1e3f5")).toContainText("waiting for their approval");
  await page.getByRole("switch", { name: /Reachable from other networks/ }).click();
  await expect(page.getByText("pv-5c0ffee15ea1ab1e@203.0.113.7:7071")).toBeVisible();
});

test("peers: a machine behind CGNAT stays reachable through a relay", async ({ page }) => {
  await page.getByRole("textbox", { name: "Relay address" }).fill("relay.example.com:7073");
  await page.getByRole("button", { name: "Use relay" }).click();
  await expect(page.getByText("pv-5c0ffee15ea1ab1e@relay://relay.example.com:7073")).toBeVisible();
  await shot(page, "relay");
});
