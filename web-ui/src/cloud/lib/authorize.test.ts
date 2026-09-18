import { describe, expect, it } from "vitest";
import {
  ApiRequestError,
  approveHeaders,
  buildConnectorStartRequest,
  decisionTarget,
  describeScope,
  fundContinuationUrl,
  isAuthorizePath,
  parseAuthorizeRequest,
  providerName,
  signerRequired,
} from "./authorize";

describe("authorize helpers", () => {
  it("matches only the consent page path", () => {
    expect(isAuthorizePath("/authorize")).toBe(true);
    expect(isAuthorizePath("/authorize/")).toBe(true);
    expect(isAuthorizePath("/oauth/authorize")).toBe(false);
    expect(isAuthorizePath("/authorized")).toBe(false);
  });

  it("reads a well-formed request id and rejects junk", () => {
    const id = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    expect(parseAuthorizeRequest(`?request=${id}`)).toBe(id);
    expect(parseAuthorizeRequest("?request=short")).toBeNull();
    expect(parseAuthorizeRequest("?request=has%20space%20and%20more%20chars")).toBeNull();
    expect(parseAuthorizeRequest("")).toBeNull();
  });

  it("describes the mcp scope in plain words", () => {
    expect(describeScope("mcp")).toContain("pay for API calls");
    expect(describeScope("mcp other")).toContain("other scope");
  });

  it("builds the wallet-creation request for the consent page", () => {
    expect(buildConnectorStartRequest("req_1", "openfort")).toEqual({
      provider: "openfort",
      authorization_request: "req_1",
    });
    expect(() => buildConnectorStartRequest("", "openfort")).toThrow();
    expect(() => buildConnectorStartRequest("req_1", "")).toThrow();
    expect(providerName("openfort")).toBe("Openfort");
    expect(providerName("acme")).toBe("acme");
  });

  it("recognises signer_required and what to add", () => {
    const err = new ApiRequestError("add pay", "signer_required", 409, {
      address: "Fg6Pa",
      signer_id: "kq_pay",
      policy_id: "pol_1",
    });
    expect(signerRequired(err)).toEqual({
      address: "Fg6Pa",
      signerId: "kq_pay",
      policyIds: ["pol_1"],
    });
    const noPolicy = new ApiRequestError("add pay", "signer_required", 409, {
      address: "Fg6Pa",
      signer_id: "kq_pay",
      policy_id: null,
    });
    expect(signerRequired(noPolicy)?.policyIds).toEqual([]);
    expect(signerRequired(new ApiRequestError("no", "no_wallet", 409, undefined))).toBeNull();
    expect(signerRequired(new ApiRequestError("x", "signer_required", 409, {}))).toBeNull();
    expect(signerRequired(new Error("plain"))).toBeNull();
  });

  it("sends the Privy token only when there is one", () => {
    expect(approveHeaders("tok")).toEqual({ authorization: "Bearer tok" });
    expect(approveHeaders(null)).toEqual({});
    expect(approveHeaders(undefined)).toEqual({});
  });

  it("routes a decision to the host, or to funding first", () => {
    expect(decisionTarget({ redirect: "https://claude.ai/cb?code=x" }, "Claude")).toBe(
      "https://claude.ai/cb?code=x",
    );
    expect(decisionTarget({ fund: { address: "Fg6Pa", request: "req_1" } }, "Claude")).toBe(
      "/fund?address=Fg6Pa&request=req_1&client=Claude",
    );
    expect(fundContinuationUrl({ address: "Fg6Pa", request: "req_1" }, "")).toBe(
      "/fund?address=Fg6Pa&request=req_1",
    );
    expect(() => decisionTarget({}, "Claude")).toThrow();
  });
});
