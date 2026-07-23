import { describe, expect, test } from "bun:test";

import { buildHubConfig } from "./config.ts";
import {
  bytesToTmuxKeyName,
  COMMAND_LAYER,
  renderAlacrittyBindings,
  renderWezTermBindings,
} from "./command-layer.ts";

describe("bytesToTmuxKeyName", () => {
  test("a plain printable byte maps to M-<byte>", () => {
    expect(bytesToTmuxKeyName("j")).toBe("M-j");
    expect(bytesToTmuxKeyName(";")).toBe("M-;");
    expect(bytesToTmuxKeyName("P")).toBe("M-P");
    expect(bytesToTmuxKeyName(".")).toBe("M-.");
  });

  test("carriage return maps to the mnemonic M-Enter", () => {
    expect(bytesToTmuxKeyName("\r")).toBe("M-Enter");
  });

  test("tab maps to the mnemonic M-Tab", () => {
    expect(bytesToTmuxKeyName("\t")).toBe("M-Tab");
  });

  test("DEL maps to the mnemonic M-BSpace", () => {
    expect(bytesToTmuxKeyName("\x7f")).toBe("M-BSpace");
  });
});

/**
 * A binding for `tmuxKey` (e.g. "M-j", "M-;", "M-BSpace") appears in a
 * generated tmux config either bare (`bind -n M-j `) or quoted by
 * `tmuxQuote` (`bind -n 'M-;' `) — tolerate either delimiter.
 */
function hasBinding(config: string, tmuxKey: string): boolean {
  const escaped = tmuxKey.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`bind -n ['"]?${escaped}['"]? `).test(config);
}

describe("command layer / outer tmux config drift guard", () => {
  test("every COMMAND_LAYER entry's tmuxKey has a matching bind in buildHubConfig()", () => {
    const hubConfig = buildHubConfig();
    for (const e of COMMAND_LAYER) {
      expect(hasBinding(hubConfig, e.tmuxKey)).toBe(true);
    }
  });

  test("tmuxKeys are unique — a typo'd byte can't silently alias another chord", () => {
    // Without this, a chord whose bytes drift onto another entry's
    // (e.g. focus-tasks accidentally emitting "d") would still pass
    // the existence check above because the OTHER binding exists.
    const keys = COMMAND_LAYER.map((e) => e.tmuxKey);
    expect(new Set(keys).size).toBe(keys.length);
  });
});

describe("renderAlacrittyBindings", () => {
  const out = renderAlacrittyBindings();

  test("emits a chars = ESC-prefixed entry for representative chords", () => {
    expect(out).toContain('key = "J", mods = "Command", chars = "\\u001bj"');
    expect(out).toContain('key = "Key1", mods = "Command", chars = "\\u001b1"');
    expect(out).toContain('key = "L", mods = "Command", chars = "\\u001b\\r"');
    expect(out).toContain('key = "E", mods = "Command", chars = "\\u001b\\t"');
    expect(out).toContain('key = "M", mods = "Command|Shift", chars = "\\u001bM"');
    expect(out).toContain('key = "Back", mods = "Command", chars = "\\u001b\\u007f"');
    expect(out).toContain('key = "Semicolon", mods = "Command", chars = "\\u001b;"');
  });

  test("wraps the entries in a [keyboard] bindings block", () => {
    expect(out).toContain("[keyboard]");
    expect(out).toContain("bindings = [");
  });

  test("emits exactly one binding line per COMMAND_LAYER entry", () => {
    const lines = out.split("\n").filter((l) => l.includes("chars = "));
    expect(lines.length).toBe(COMMAND_LAYER.length);
  });

  test("documents the deliberately-unbound keys and known costs", () => {
    expect(out).toContain("cmd+n");
    expect(out).toContain("cmd+h");
    expect(out).toContain("clear-scrollback");
  });

  test("notes the optional zsh cmd+w widget", () => {
    expect(out).toContain("bindkey");
    expect(out).toContain("zle");
  });
});

describe("renderWezTermBindings", () => {
  const out = renderWezTermBindings();

  test("emits a SendString entry for representative chords", () => {
    expect(out).toContain("{ key = 'j', mods = 'CMD', action = wezterm.action.SendString '\\x1bj' }");
    expect(out).toContain("{ key = '1', mods = 'CMD', action = wezterm.action.SendString '\\x1b1' }");
    expect(out).toContain("{ key = 'l', mods = 'CMD', action = wezterm.action.SendString '\\x1b\\r' }");
    expect(out).toContain(
      "{ key = 'm', mods = 'CMD|SHIFT', action = wezterm.action.SendString '\\x1bM' }",
    );
    expect(out).toContain(
      "{ key = 'Backspace', mods = 'CMD', action = wezterm.action.SendString '\\x1b\\x7f' }",
    );
  });

  test("punctuation chords keep WezTerm's literal key names (not Alacritty's)", () => {
    // WezTerm takes the literal character (key = ';'), unlike
    // Alacritty's Semicolon/Period/Slash names — a well-meaning
    // "consistency" fix here would silently break the bindings.
    expect(out).toContain("{ key = ';', mods = 'CMD'");
    expect(out).toContain("{ key = '.', mods = 'CMD'");
    expect(out).toContain("{ key = '/', mods = 'CMD'");
  });

  test("emits exactly one binding line per COMMAND_LAYER entry", () => {
    const lines = out.split("\n").filter((l) => l.includes("SendString"));
    // One header line mentions SendString in prose — count only real
    // table entries.
    const entries = lines.filter((l) => l.trimStart().startsWith("{ key"));
    expect(entries.length).toBe(COMMAND_LAYER.length);
  });

  test("defines the wt_hub_keys table and shows how to merge it in", () => {
    expect(out).toContain("local wt_hub_keys = {");
    expect(out).toContain("config.keys");
  });

  test("documents shadowed WezTerm defaults and the deliberately-unbound keys", () => {
    expect(out).toContain("SpawnTab");
    expect(out).toContain("CloseCurrentTab");
    expect(out).toContain("ClearScrollback");
    expect(out).toContain("ActivateTab");
    expect(out).toContain("cmd+n");
    expect(out).toContain("cmd+h");
  });
});
