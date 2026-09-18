import { describe, expect, it } from "vitest";
import {
  buildFundStartRequest,
  buildReturnUrl,
  explorerTxUrl,
  formatUsd,
  isFundPath,
  parseCoinflowMessage,
  parseFundParams,
  shortAddress,
  continueLabel,
  isConnectorFunding,
} from "./fund";

const ADDRESS = "CcZFhGwFVkZevr555EZJpWbeq4irboT6zHfrSKWKCy3Z";
const CALLBACK = "http://127.0.0.1:53211/callback";

describe("parseFundParams", () => {
  it("reads the CLI parameters and drops empties", () => {
    const p = parseFundParams(
      `?address=${ADDRESS}&callback=${encodeURIComponent(CALLBACK)}&state=abc&account=&cli=0.29.0&cents=2000`,
    );
    expect(p).toEqual({
      address: ADDRESS,
      callback: CALLBACK,
      state: "abc",
      account: undefined,
      cli: "0.29.0",
      cents: 2000,
    });
  });

  it("ignores a malformed amount", () => {
    expect(parseFundParams("?cents=abc").cents).toBeUndefined();
    expect(parseFundParams("?cents=-5").cents).toBeUndefined();
    expect(parseFundParams("").cents).toBeUndefined();
  });
});

describe("isFundPath", () => {
  it("matches only the funding page", () => {
    expect(isFundPath("/fund")).toBe(true);
    expect(isFundPath("/fund/")).toBe(true);
    expect(isFundPath("/funds")).toBe(false);
    expect(isFundPath("/onboard")).toBe(false);
  });
});

describe("buildFundStartRequest", () => {
  it("carries the address, amount and optional CLI return info", () => {
    expect(
      buildFundStartRequest({ address: ADDRESS, callback: CALLBACK, state: "s" }, 2000),
    ).toEqual({ address: ADDRESS, cents: 2000, callback: CALLBACK, state: "s" });
    expect(buildFundStartRequest({ address: ADDRESS }, 1000)).toEqual({
      address: ADDRESS,
      cents: 1000,
    });
  });

  it("refuses to post without an address", () => {
    expect(() => buildFundStartRequest({}, 2000)).toThrow("missing address");
  });
});

describe("buildReturnUrl", () => {
  it("appends state, payment id and signature to the callback", () => {
    const url = new URL(buildReturnUrl(CALLBACK, "st4te", "pay_1", "5ig"));
    expect(url.origin + url.pathname).toBe(CALLBACK);
    expect(url.searchParams.get("state")).toBe("st4te");
    expect(url.searchParams.get("payment_id")).toBe("pay_1");
    expect(url.searchParams.get("signature")).toBe("5ig");
  });

  it("keeps existing query parameters and omits what it does not have", () => {
    const url = new URL(buildReturnUrl(`${CALLBACK}?keep=1`, undefined, "pay_1"));
    expect(url.searchParams.get("keep")).toBe("1");
    expect(url.searchParams.has("state")).toBe(false);
    expect(url.searchParams.has("signature")).toBe(false);
  });
});

describe("parseCoinflowMessage", () => {
  const ORIGIN = "https://sandbox.coinflow.cash";

  it("drops messages from other origins", () => {
    expect(parseCoinflowMessage('{"data":"success"}', "https://evil.test", ORIGIN)).toBeNull();
  });

  it("reads height changes from strings and objects", () => {
    expect(parseCoinflowMessage('{"method":"heightChange","data":"640"}', ORIGIN, ORIGIN)).toEqual({
      kind: "height",
      px: 640,
    });
    expect(parseCoinflowMessage({ method: "heightChange", data: 512 }, ORIGIN, ORIGIN)).toEqual({
      kind: "height",
      px: 512,
    });
    expect(parseCoinflowMessage({ method: "heightChange", data: "nope" }, ORIGIN, ORIGIN)).toBeNull();
  });

  it("reads success with the payment id", () => {
    expect(
      parseCoinflowMessage('{"data":"success","info":{"paymentId":"pay_1"}}', ORIGIN, ORIGIN),
    ).toEqual({ kind: "success", paymentId: "pay_1" });
    expect(parseCoinflowMessage({ data: "success" }, ORIGIN, ORIGIN)).toEqual({
      kind: "success",
      paymentId: undefined,
    });
  });

  it("reads errors and ignores everything else", () => {
    expect(
      parseCoinflowMessage({ data: "error", info: { message: "declined" } }, ORIGIN, ORIGIN),
    ).toEqual({ kind: "error", message: "declined" });
    expect(parseCoinflowMessage("not json", ORIGIN, ORIGIN)).toBeNull();
    expect(parseCoinflowMessage({ hello: 1 }, ORIGIN, ORIGIN)).toBeNull();
    expect(parseCoinflowMessage(42, ORIGIN, ORIGIN)).toBeNull();
  });
});

describe("formatting", () => {
  it("formats cents as dollars", () => {
    expect(formatUsd(2170)).toBe("$21.70");
    expect(formatUsd(5)).toBe("$0.05");
    expect(formatUsd(100000)).toBe("$1,000.00");
    expect(formatUsd(-250)).toBe("-$2.50");
  });

  it("shortens addresses and builds explorer links", () => {
    expect(shortAddress(ADDRESS)).toBe("CcZF…Cy3Z");
    expect(shortAddress("short")).toBe("short");
    expect(explorerTxUrl("5ig", "production")).toBe("https://explorer.solana.com/tx/5ig");
    expect(explorerTxUrl("5ig", "sandbox")).toBe(
      "https://explorer.solana.com/tx/5ig?cluster=devnet",
    );
  });

  it("recognises the connector continuation", () => {
    const p = parseFundParams("?address=Fg6Pa&request=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk&client=Claude");
    expect(p.request).toBe("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    expect(isConnectorFunding(p)).toBe(true);
    expect(continueLabel(p)).toBe("Continue to Claude");
    expect(isConnectorFunding(parseFundParams("?address=Fg6Pa"))).toBe(false);
    expect(isConnectorFunding(parseFundParams("?address=Fg6Pa&request=short"))).toBe(false);
    expect(continueLabel(parseFundParams("?address=Fg6Pa"))).toBe("Continue to your MCP client");
  });
});
