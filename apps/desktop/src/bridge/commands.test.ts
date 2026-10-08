import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { BridgeError, commands, inTauri } from "./commands";

describe("command bridge", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("falls back to the mock outside Tauri", () => {
    expect(inTauri).toBe(false);
  });

  it("wraps backend errors in BridgeError with the error code", async () => {
    const p = commands.deployInstance("missing-offer", { vcpus: 1, memMib: 512, diskGib: 10, image: "alpine-3.22", confidential: false });
    const assertion = expect(p).rejects.toSatisfy((e: unknown) => e instanceof BridgeError && e.code === "not_found");
    await vi.advanceTimersByTimeAsync(100);
    await assertion;
  });
});
