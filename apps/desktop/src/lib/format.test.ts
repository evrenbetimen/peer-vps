import { describe, expect, it } from "vitest";

import { bytesPerSec, credits, cx, mib, perHour } from "./format";

describe("format", () => {
  it("renders µcredits as credits", () => {
    expect(credits(1_500_000)).toMatch(/^1[.,]50$/);
    expect(credits(-250_000, 4)).toMatch(/^-0[.,]2500$/);
    expect(credits(0)).toMatch(/^0[.,]00$/);
  });

  it("prices per hour from per-second rates", () => {
    // 1200 µcr/s × 3600 = 4.32 cr/h
    expect(perHour(1200)).toMatch(/^4[.,]32 cr\/h$/);
  });

  it("formats memory sizes", () => {
    expect(mib(512)).toBe("512 MiB");
    expect(mib(1024)).toBe("1 GiB");
    expect(mib(1536)).toBe("1.5 GiB");
    expect(mib(32768)).toBe("32 GiB");
  });

  it("scales throughput units", () => {
    expect(bytesPerSec(0)).toBe("0.0 B/s");
    expect(bytesPerSec(999)).toBe("999 B/s");
    expect(bytesPerSec(2_500_000)).toBe("2.5 MB/s");
    expect(bytesPerSec(5e12)).toBe("5000 GB/s");
  });

  it("joins only truthy class names", () => {
    expect(cx("a", false, null, undefined, "b")).toBe("a b");
  });
});
