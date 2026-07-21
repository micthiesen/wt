import { describe, expect, test } from "bun:test";

import {
  decodeRemoteArgs,
  encodeRemoteArgs,
  remoteWtCommand,
} from "./remote-protocol.ts";

const remote = {
  host: "cachy",
  label: "cachy",
  wtPath: "~/.wt/bin/wt",
};

describe("remote argv protocol", () => {
  test("round-trips shell-significant arguments exactly", () => {
    const argv = ["new", "eng-123?a=1&b='two words'", "--base", "feature/x"];
    expect(decodeRemoteArgs(encodeRemoteArgs(argv))).toEqual(argv);
  });

  test("rejects malformed payloads", () => {
    expect(() => decodeRemoteArgs("not-json")).toThrow("invalid remote argv payload");
    const object = Buffer.from(JSON.stringify({ nope: true })).toString("base64url");
    expect(() => decodeRemoteArgs(object)).toThrow("array of strings");
  });
});

describe("remoteWtCommand", () => {
  test("expands the default path on the remote and hides raw argv from the shell", () => {
    const command = remoteWtCommand(remote, ["new", "a&b"]);
    expect(command).toStartWith('exec "$HOME"/\'.wt/bin/wt\' _remote ');
    expect(command).not.toContain("a&b");
  });

  test("launches the interactive TUI without an argv payload", () => {
    expect(remoteWtCommand(remote, null)).toBe('exec "$HOME"/\'.wt/bin/wt\'');
  });
});
