import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

// xterm needs a real canvas; the terminal itself is covered by the Playwright suite.
vi.mock("./components/Terminal", () => ({
  Terminal: ({ instance }: { instance: { virtualIp: string } }) => <div data-testid="terminal">shell {instance.virtualIp}</div>,
}));

const { App } = await import("./App");

describe("App", () => {
  it("navigates between the four views", async () => {
    const user = userEvent.setup();
    render(<App />);
    expect(screen.getByText("browser preview (mock data)")).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("Provider mode");
    expect(await screen.findByText("Resources offered to the network")).toBeInTheDocument();

    for (const [nav, heading] of [
      ["Console", "Renter & agents"],
      ["Wallet", "Credits & billing"],
      ["Failover", "Topology & HA"],
      ["Host", "Provider mode"],
    ] as const) {
      await user.click(screen.getByRole("button", { name: new RegExp(`^${nav}`) }));
      expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(heading);
      expect(screen.getByRole("button", { name: new RegExp(`^${nav}`) })).toHaveAttribute("aria-current", "page");
    }
  });

  it("deploys, scales and terminates an instance from the renter console", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: /^Console/ }));
    const deploy = await screen.findByRole("button", { name: "Deploy" });
    await vi.waitFor(() => expect(deploy).toBeEnabled());
    await user.click(deploy);

    expect(await screen.findByTestId("terminal")).toHaveTextContent(/shell 10\.147\.0\.\d+/);
    expect(await screen.findByText(/1 active/)).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: /^ssh -p \d+ peervps@127\.0\.0\.1$/ })).toBeInTheDocument();
    expect(screen.getByText("mock-password")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Scale to 0" }));
    expect(await screen.findByText("scaledToZero")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Resume" }));
    expect(await screen.findByText("running")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Terminate" }));
    expect(await screen.findByText(/0 active/)).toBeInTheDocument();
  });

  it("filters offers when a GPU template and VRAM floor are picked", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: /^Console/ }));
    await user.click(screen.getByRole("button", { name: /Ubuntu \+ CUDA/ }));
    await user.click(within(screen.getByRole("group", { name: "Max price" })).getByRole("button", { name: "500 cr/h" }));
    await user.click(within(screen.getByRole("group", { name: "Min VRAM" })).getByRole("button", { name: "40 GiB" }));
    expect(await screen.findByText(/3 · Offers · 1/)).toBeInTheDocument();
    expect(screen.getByText("iad-h100-3")).toBeInTheDocument();
  });

  it("tops up the wallet and shows the ledger entry", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: /^Wallet/ }));
    await user.click(await screen.findByRole("button", { name: "+25 cr" }));
    expect(await screen.findByText("Top-up")).toBeInTheDocument();
  });

  it("lists guest images and downloads one from the catalog", async () => {
    const user = userEvent.setup();
    render(<App />);
    const debian = await screen.findByTestId("image-debian-13");
    expect(within(await screen.findByTestId("image-ubuntu-24.04")).getByText(/installed/)).toBeInTheDocument();
    await user.click(within(debian).getByRole("button", { name: "Download" }));
    expect(await within(screen.getByTestId("image-debian-13")).findByText(/downloading/)).toBeInTheDocument();
    expect(await within(screen.getByTestId("image-debian-13")).findByText(/installed/, {}, { timeout: 5000 })).toBeInTheDocument();
  });

  it("adds a Windows ISO and deploys it with a screen and Remote Desktop", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "Add ISO or disk…" }));
    const row = await screen.findByTestId("image-win11_24h2_english_arm64");
    expect(within(row).getByText("copying…")).toBeInTheDocument();
    expect(await within(row).findByText(/installed/, {}, { timeout: 5000 })).toBeInTheDocument();
    expect(within(screen.getByTestId("image-win11_24h2_english_arm64")).getByText("Windows installer · ARM64")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /^Console/ }));
    await user.click(await screen.findByRole("button", { name: /win11_24h2_english_arm64/ }, { timeout: 6000 }));
    expect(within(screen.getByRole("group", { name: "Disk" })).getByRole("button", { name: "80 GiB" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByText(/Windows signs in the user shown there/)).toBeInTheDocument();
    const deploy = screen.getByRole("button", { name: "Deploy" });
    await vi.waitFor(() => expect(deploy).toBeEnabled());
    await user.click(deploy);

    expect(await screen.findByText("Remote Desktop")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^127\.0\.0\.1:\d+$/ })).toBeInTheDocument();
    expect(screen.getByText("screen password")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Open screen" }));
    expect(screen.queryByText(/no screen/)).not.toBeInTheDocument();
  }, 20_000);

  it("adds a peer, approves one that asked, and rents a peer's machine", async () => {
    const user = userEvent.setup();
    render(<App />);
    expect(await screen.findByText("pv-5c0ffee15ea1ab1e@192.168.1.10:7071")).toBeInTheDocument();

    await user.type(screen.getByRole("textbox", { name: "Peer address" }), "pv-0123456789abcdef@192.168.1.40:7071");
    await user.click(screen.getByRole("button", { name: "Add peer" }));
    expect(await within(await screen.findByTestId("peer-pv-0123456789abcdef")).findByText("waiting for their approval")).toBeInTheDocument();

    const asking = screen.getByTestId("peer-pv-a17b0c55e9d24f13");
    expect(within(asking).getByText(/wants to rent from you/)).toBeInTheDocument();
    await user.click(within(asking).getByRole("button", { name: "Approve" }));
    await vi.waitFor(() => expect(within(screen.getByTestId("peer-pv-a17b0c55e9d24f13")).getByText("online")).toBeInTheDocument());

    await user.click(screen.getByRole("button", { name: /^Console/ }));
    await user.click(await screen.findByText("pv-3f9c1a7e2b4d6c80/this-machine"));
    const deploy = screen.getByRole("button", { name: "Deploy" });
    await vi.waitFor(() => expect(deploy).toBeEnabled());
    await user.click(deploy);
    expect(await screen.findByText(/on pv-3f9c1a7e2b4d6c80/)).toBeInTheDocument();
  });

  it("adds a machine found on the network and opens a port for other networks", async () => {
    const user = userEvent.setup();
    render(<App />);
    const near = await screen.findByTestId("nearby-pv-b2e4f6a8c0d1e3f5");
    await user.click(within(near).getByRole("button", { name: "Add" }));
    expect(await within(await screen.findByTestId("peer-pv-b2e4f6a8c0d1e3f5")).findByText("waiting for their approval")).toBeInTheDocument();
    expect(screen.queryByTestId("nearby-pv-b2e4f6a8c0d1e3f5")).not.toBeInTheDocument();

    expect(screen.getByText(/only machines on this network can add this one/)).toBeInTheDocument();
    await user.click(screen.getByRole("switch", { name: /Reachable from other networks/ }));
    expect(await screen.findByText("pv-5c0ffee15ea1ab1e@203.0.113.7:7071", {}, { timeout: 5000 })).toBeInTheDocument();
  });

  it("stays reachable through a relay and stops", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.type(await screen.findByRole("textbox", { name: "Relay address" }), "relay.example.com");
    await user.click(screen.getByRole("button", { name: "Use relay" }));
    expect(await screen.findByText("pv-5c0ffee15ea1ab1e@relay://relay.example.com:7073", {}, { timeout: 5000 })).toBeInTheDocument();
    expect(screen.getByText("connected")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Stop" }));
    expect(await screen.findByRole("textbox", { name: "Relay address" })).toBeInTheDocument();
  });
});
