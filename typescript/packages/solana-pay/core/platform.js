// @ts-check
"use strict";

const os = require("os");
const { spawnSync } = require("child_process");

/**
 * Resolve the current platform to a Rust target triple that matches a
 * `supportedPlatforms` entry in package.json.
 *
 * @returns {string} Rust target triple (e.g. "aarch64-apple-darwin")
 */
function getPlatform() {
  const pkg = require("./package.json");
  const supportedPlatforms = pkg.supportedPlatforms;

  const rawArch = os.arch();
  const rawOs = os.type();

  // Map Node arch → Rust arch
  const archMap = {
    arm64: "aarch64",
    x64: "x86_64",
  };
  const arch = archMap[rawArch];
  if (!arch) {
    throw new Error(`Unsupported architecture: ${rawArch}`);
  }

  // Map Node os.type() → Rust target OS suffix
  let osType;
  switch (rawOs) {
    case "Darwin":
      osType = "apple-darwin";
      break;
    case "Linux":
      osType = detectLinuxLibc();
      break;
    case "Windows_NT":
      osType = "pc-windows-msvc";
      break;
    default:
      throw new Error(`Unsupported OS: ${rawOs}`);
  }

  const key = `${arch}-${osType}`;

  if (supportedPlatforms[key]) {
    return key;
  }

  // Fallback: try musl if glibc variant not available on Linux
  if (rawOs === "Linux" && !key.includes("musl")) {
    const muslKey = `${arch}-unknown-linux-musl`;
    if (supportedPlatforms[muslKey]) {
      return muslKey;
    }
  }

  throw new Error(
    `Unsupported platform: ${key}\nSupported: ${Object.keys(supportedPlatforms).join(", ")}`,
  );
}

/**
 * Detect whether the Linux system uses musl or glibc.
 * @returns {string}
 */
function detectLinuxLibc() {
  try {
    const result = spawnSync("ldd", ["--version"], {
      encoding: "utf-8",
      timeout: 5000,
    });
    const output = ((result.stdout || "") + (result.stderr || "")).toLowerCase();
    if (output.includes("musl")) {
      return "unknown-linux-musl";
    }
  } catch {
    // ldd not available — fall through to glibc default
  }
  return "unknown-linux-gnu";
}

module.exports = { getPlatform };
