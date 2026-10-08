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
});
