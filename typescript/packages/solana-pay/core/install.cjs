// @ts-check
'use strict';

const fs = require('fs');
const path = require('path');
const https = require('https');
const { createHash } = require('crypto');
const childProcess = require('child_process');
const { execSync } = childProcess;
const { getPlatform } = require('./platform.cjs');

const pkg = require('./package.json');
const BIN_DIR = path.join(__dirname, 'bin');
const VERSION_FILE = path.join(BIN_DIR, '.version');

function main() {
    const version = pkg.cliVersion || resolveLatestCliVersion();

    // Skip if already installed at the correct version
    if (fs.existsSync(VERSION_FILE) && fs.readFileSync(VERSION_FILE, 'utf-8').trim() === version) {
        return;
    }

    const platform = getPlatform();
    const meta = pkg.supportedPlatforms[platform];
    if (!meta) {
        console.error(`No binary available for platform: ${platform}`);
        process.exit(1);
    }

    const baseUrl = `https://github.com/solana-foundation/pay/releases/download/pay-v${version}`;
    const artifactUrl = `${baseUrl}/${meta.artifact}`;
    const shaUrl = `${baseUrl}/sha256sums.txt`;

    fs.mkdirSync(BIN_DIR, { recursive: true });

    const artifactPath = path.join(BIN_DIR, meta.artifact);

    console.log(`Downloading pay v${version} for ${platform}...`);

    // Download artifact
    downloadFile(artifactUrl, artifactPath);

    // Verify checksum
    try {
        const sums = downloadToString(shaUrl);
        const line = sums.split('\n').find(l => l.includes(meta.artifact));
        if (line) {
            const expectedSha = line.trim().split(/\s+/)[0];
            const actualSha = sha256File(artifactPath);
            if (actualSha !== expectedSha) {
                console.error(`Checksum mismatch!\n  Expected: ${expectedSha}\n  Actual:   ${actualSha}`);
                fs.unlinkSync(artifactPath);
                process.exit(1);
            }
            console.log('Checksum verified.');
        }
    } catch (err) {
        console.warn(`Checksum file not available, skipping verification: ${err.message}`);
    }

    // Extract
    if (meta.artifact.endsWith('.tar.gz')) {
        execSync(`tar xzf "${artifactPath}" -C "${BIN_DIR}"`, { stdio: 'inherit' });
    } else if (meta.artifact.endsWith('.zip')) {
        extractZip(artifactPath, BIN_DIR);
    }

    // Set executable permission on Unix
    if (process.platform !== 'win32') {
        const binPath = path.join(BIN_DIR, meta.binary);
        fs.chmodSync(binPath, 0o755);
    }

    // Clean up archive
    fs.unlinkSync(artifactPath);

    // Write version marker
    fs.writeFileSync(VERSION_FILE, version);

    console.log(`pay v${version} installed successfully.`);
}

/**
 * Extract a zip without depending on Unix `unzip`, which Windows does not ship.
 * Prefer `%SystemRoot%\System32\tar.exe` (bsdtar) so Git/MSYS GNU tar on PATH
 * cannot steal the lookup — GNU tar cannot read zip archives.
 *
 * @param {string} artifactPath
 * @param {string} destDir
 */
function extractZip(artifactPath, destDir) {
    if (process.platform === 'win32') {
        extractZipWindows(artifactPath, destDir);
        return;
    }
    childProcess.execSync(`unzip -o "${artifactPath}" -d "${destDir}"`, { stdio: 'inherit' });
}

/**
 * @param {string} artifactPath
 * @param {string} destDir
 */
function extractZipWindows(artifactPath, destDir) {
    const tar = windowsTarCandidates(process.env).find(candidate => fs.existsSync(candidate));

    if (tar) {
        childProcess.execFileSync(tar, ['-xf', artifactPath, '-C', destDir], { stdio: 'inherit' });
        return;
    }

    const quotedArtifact = quotePowerShellLiteral(artifactPath);
    const quotedDest = quotePowerShellLiteral(destDir);
    childProcess.execFileSync(
        'powershell.exe',
        [
            '-NoProfile',
            '-NonInteractive',
            '-Command',
            `Expand-Archive -LiteralPath '${quotedArtifact}' -DestinationPath '${quotedDest}' -Force`,
        ],
        { stdio: 'inherit' },
    );
}

/**
 * @param {NodeJS.ProcessEnv} [env]
 * @returns {string[]}
 */
function windowsTarCandidates(env) {
    const systemRoot = (env && (env.SystemRoot || env.WINDIR)) || 'C:\\Windows';
    return [
        path.join(systemRoot, 'System32', 'tar.exe'),
        // 32-bit Node on 64-bit Windows: System32 is redirected to SysWOW64.
        path.join(systemRoot, 'Sysnative', 'tar.exe'),
    ];
}

/**
 * @param {string} value
 * @returns {string}
 */
function quotePowerShellLiteral(value) {
    return value.replace(/'/g, "''");
}

/**
 * Query the GitHub API for the latest pay-v* release tag.
 * @returns {string}
 */
function resolveLatestCliVersion() {
    const json = downloadToString('https://api.github.com/repos/solana-foundation/pay/releases?per_page=20');
    const releases = JSON.parse(json);
    const release = releases.find(r => r.tag_name && r.tag_name.startsWith('pay-v') && !r.draft);
    if (!release) {
        throw new Error('No CLI release found on GitHub');
    }
    return release.tag_name.replace('pay-v', '');
}

/**
 * Download a file from a URL, following redirects (GitHub releases redirect).
 * @param {string} url
 * @param {string} dest
 */
function downloadFile(url, dest) {
    const result = execSync(`curl -fsSL --retry 3 --retry-delay 2 -o "${dest}" "${url}"`, { stdio: 'inherit' });
}

/**
 * Download a URL and return its contents as a string.
 * @param {string} url
 * @returns {string}
 */
function downloadToString(url) {
    return execSync(`curl -fsSL --retry 3 "${url}"`, { encoding: 'utf-8' });
}

/**
 * Compute SHA-256 hash of a file.
 * @param {string} filePath
 * @returns {string}
 */
function sha256File(filePath) {
    const data = fs.readFileSync(filePath);
    return createHash('sha256').update(data).digest('hex');
}

if (require.main === module) {
    try {
        main();
    } catch (err) {
        console.error(`Failed to install pay binary: ${err.message}`);
        process.exit(1);
    }
}

module.exports = {
    extractZipWindows,
    quotePowerShellLiteral,
    windowsTarCandidates,
};
