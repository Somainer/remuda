import { describe, expect, it } from "vitest";
import { NIGHT_CORRAL_THEME, nightCorralExtendedAnsi } from "./theme";

describe("Night Corral 256-color theme", () => {
  it("maps ink/paper/dust tokens and fills ANSI 16–255", () => {
    expect(NIGHT_CORRAL_THEME.background).toBe("#12161C");
    expect(NIGHT_CORRAL_THEME.foreground).toBe("#E7DCC8");
    expect(NIGHT_CORRAL_THEME.cursor).toBe("#C9842A");
    expect(nightCorralExtendedAnsi()).toHaveLength(240);
    expect(NIGHT_CORRAL_THEME.extendedAnsi).toHaveLength(240);
  });
});
