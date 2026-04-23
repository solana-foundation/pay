#!/usr/bin/env node
// @ts-check
'use strict';

const path = require('path');
const fs = require('fs');
const { spawnSync } = require('child_process');
const { getPlatform } = require('./platform.cjs');

const pkg = require('./package.json');
const platform = getPlatform();
const meta = pkg.supportedPlatforms[platform];

if (!meta) {
    console.error(`Unsupported platform: ${platform}`);
    process.exit(1);
}

const binPath = path.join(__dirname, 'bin', meta.binary);

// Auto-install if binary is missing
if (!fs.existsSync(binPath)) {
    console.error('pay binary not found. Running install...');
    const install = spawnSync(process.execPath, [path.join(__dirname, 'install.cjs')], {
        stdio: 'inherit',
    });
    if (install.status !== 0) {
        console.error('Installation failed.');
        process.exit(install.status || 1);
    }
}

// Forward all arguments to the native binary
const result = spawnSync(binPath, process.argv.slice(2), {
    stdio: 'inherit',
});

process.exit(result.status ?? 1);
