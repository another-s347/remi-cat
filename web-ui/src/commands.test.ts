import { describe, expect, it } from "vitest";
import { commandSuggestions } from "./commands";

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
