import { describe, it, expect } from "vitest";
import {
  buildStartRequest,
  hasLinkParams,
  isValidEmail,
  parseOnboardParams,
} from "./onboard";

const LINK = {
  callback: "http://127.0.0.1:53211/callback",
  state: "abcdefghijklmnopqrstuvwxyz012345",
  code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
};

describe("isValidEmail", () => {
  it("accepts ordinary addresses", () => {
    expect(isValidEmail("a@b.co")).toBe(true);
    expect(isValidEmail("first.last+tag@example.com")).toBe(true);
    expect(isValidEmail("  padded@example.org  ")).toBe(true);
  });

  it("rejects malformed addresses", () => {
    expect(isValidEmail("")).toBe(false);
    expect(isValidEmail("nope")).toBe(false);
    expect(isValidEmail("@example.com")).toBe(false);
    expect(isValidEmail("user@")).toBe(false);
    expect(isValidEmail("user@localhost")).toBe(false);
    expect(isValidEmail("user@.com")).toBe(false);
    expect(isValidEmail("user@example.")).toBe(false);
    expect(isValidEmail("a@b@c.com")).toBe(false);
    expect(isValidEmail("has space@example.com")).toBe(false);
  });
});

describe("parseOnboardParams", () => {
  it("reads every known key and decodes percent-encoding", () => {
    const search =
      "?callback=http%3A%2F%2F127.0.0.1%3A53211%2Fcallback&state=s1234567890abcdef" +
      "&code_challenge=c&account=work&host=ludo-mbp&cli=0.29.0";
    expect(parseOnboardParams(search)).toEqual({
      callback: "http://127.0.0.1:53211/callback",
      state: "s1234567890abcdef",
      code_challenge: "c",
      account: "work",
      host: "ludo-mbp",
      cli: "0.29.0",
    });
  });

  it("drops missing and empty values", () => {
    const params = parseOnboardParams("?callback=&host=x");
    expect(params.callback).toBeUndefined();
    expect(params.state).toBeUndefined();
    expect(params.code_challenge).toBeUndefined();
    expect(params.host).toBe("x");
    expect(hasLinkParams(params)).toBe(false);
  });

  it("handles an empty query string", () => {
    expect(hasLinkParams(parseOnboardParams(""))).toBe(false);
  });

  it("requires all three link params", () => {
    expect(hasLinkParams({ callback: LINK.callback, state: LINK.state })).toBe(false);
    expect(hasLinkParams(LINK)).toBe(true);
  });
});

describe("buildStartRequest", () => {
  it("produces the wire shape with only the provided optional fields", () => {
    const req = buildStartRequest({ ...LINK, host: "ludo-mbp" }, " a@b.co ");
    expect(req).toEqual({
      email: "a@b.co",
      callback: LINK.callback,
      state: LINK.state,
      code_challenge: LINK.code_challenge,
      host: "ludo-mbp",
    });
    expect("account" in req).toBe(false);
    expect("cli" in req).toBe(false);
  });

  it("includes account and cli when present", () => {
    const req = buildStartRequest({ ...LINK, account: "default", cli: "0.29.0" }, "a@b.co");
    expect(req.account).toBe("default");
    expect(req.cli).toBe("0.29.0");
  });

  it("throws when link params are missing", () => {
    expect(() => buildStartRequest({ callback: LINK.callback }, "a@b.co")).toThrow(
      /missing callback/,
    );
  });
});

describe("providerCallbackFromPath", () => {
  it("extracts the provider from the consent callback path", async () => {
    const { providerCallbackFromPath } = await import("./onboard");
    expect(providerCallbackFromPath("/onboard/openfort/callback")).toBe("openfort");
    expect(providerCallbackFromPath("/onboard/openfort/callback/")).toBe("openfort");
    expect(providerCallbackFromPath("/onboard")).toBeNull();
    expect(providerCallbackFromPath("/onboard/Openfort/callback")).toBeNull();
    expect(providerCallbackFromPath("/other/openfort/callback")).toBeNull();
  });
});

describe("buildProviderStartRequest", () => {
  it("carries the link params and the provider, no email", async () => {
    const { buildProviderStartRequest } = await import("./onboard");
    const req = buildProviderStartRequest(
      { callback: "http://127.0.0.1:1/callback", state: "s", code_challenge: "c", host: "h" },
      "openfort",
    );
    expect(req).toEqual({
      provider: "openfort",
      callback: "http://127.0.0.1:1/callback",
      state: "s",
      code_challenge: "c",
      host: "h",
    });
    expect("email" in req).toBe(false);
  });
});
