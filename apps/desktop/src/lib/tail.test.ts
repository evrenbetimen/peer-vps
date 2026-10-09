import { describe, expect, it } from "vitest";

import { newSuffix } from "./tail";

describe("newSuffix", () => {
  it("appends what grew", () => {
    expect(newSuffix("boot\nlogin: ", "boot\nlogin: peervps\n")).toBe("peervps\n");
    expect(newSuffix("", "boot")).toBe("boot");
  });

  it("follows a tail window that moved past the start", () => {
    const shown = "boot log\n".repeat(100) + "line one\nline two\n";
    const text = shown.slice(500) + "line three\n";
    expect(newSuffix(shown, text)).toBe("line three\n");
  });

  it("gives up when the two no longer overlap", () => {
    expect(newSuffix("old output", "something else entirely")).toBeNull();
  });
});
