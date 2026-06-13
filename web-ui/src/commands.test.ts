import { describe, expect, it } from "vitest";
import { activeFileMentionToken, commandSuggestions, replaceFileMentionToken } from "./commands";

describe("commandSuggestions", () => {
  it("only opens for slash-prefixed single-line input", () => {
    expect(commandSuggestions("hello")).toEqual([]);
    expect(commandSuggestions("/goal\nnext")).toEqual([]);
    expect(commandSuggestions("/").length).toBeGreaterThan(5);
  });

  it("filters command names and localized keywords", () => {
    expect(commandSuggestions("/goal set")[0]?.value).toBe("/goal set ");
    expect(commandSuggestions("/诊断")[0]?.value).toBe("/doctor");
    expect(commandSuggestions("/workflow stop")[0]?.value).toBe("/workflow stop");
  });
});

describe("file mention helpers", () => {
  it("detects the active @ token before the cursor", () => {
    expect(activeFileMentionToken("@src")).toEqual({ start: 0, end: 4, query: "src" });
    expect(activeFileMentionToken("read @src/ma")).toEqual({ start: 5, end: 12, query: "src/ma" });
    expect(activeFileMentionToken("read @src/main.rs now", 17)).toEqual({
      start: 5,
      end: 17,
      query: "src/main.rs",
    });
    expect(activeFileMentionToken("email a@b.com")).toBeUndefined();
    expect(activeFileMentionToken("read @src now")).toBeUndefined();
  });

  it("replaces only the active token", () => {
    const input = "read @src then";
    const token = activeFileMentionToken(input, "read @src".length);
    expect(token).toBeDefined();
    expect(replaceFileMentionToken(input, token!, "src/main.rs")).toBe(
      "read @src/main.rs then",
    );
  });
});
