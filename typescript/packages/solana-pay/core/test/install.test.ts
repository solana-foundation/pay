import { createRequire } from 'node:module';
import path from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const require = createRequire(import.meta.url);
const fs = require('fs') as typeof import('fs');
const childProcess = require('child_process') as typeof import('child_process');
const { extractZipWindows, quotePowerShellLiteral, windowsTarCandidates } = require('../install.cjs') as {
    extractZipWindows: (artifactPath: string, destDir: string) => void;
    quotePowerShellLiteral: (value: string) => string;
    windowsTarCandidates: (env?: NodeJS.ProcessEnv) => string[];
};

const ARTIFACT = path.join("C:\\Users\\O'Brien", 'pay zip', 'pay-x86_64-pc-windows-msvc.zip');
const DEST = path.join("C:\\Users\\O'Brien", 'pay zip', 'bin');
const originalSystemRoot = process.env.SystemRoot;
const originalWindir = process.env.WINDIR;

describe('windowsTarCandidates', () => {
    it('resolves System32 then Sysnative from SystemRoot', () => {
        expect(windowsTarCandidates({ SystemRoot: 'C:\\Windows' })).toEqual([
            path.join('C:\\Windows', 'System32', 'tar.exe'),
            path.join('C:\\Windows', 'Sysnative', 'tar.exe'),
        ]);
    });

    it('falls back to WINDIR then C:\\Windows', () => {
        expect(windowsTarCandidates({ WINDIR: 'D:\\Windows' })[0]).toBe(
            path.join('D:\\Windows', 'System32', 'tar.exe'),
        );
        expect(windowsTarCandidates({})[0]).toBe(path.join('C:\\Windows', 'System32', 'tar.exe'));
    });
});

describe('extractZipWindows', () => {
    beforeEach(() => {
        process.env.SystemRoot = 'C:\\Windows';
        delete process.env.WINDIR;
    });

    afterEach(() => {
        vi.restoreAllMocks();
        if (originalSystemRoot === undefined) {
            delete process.env.SystemRoot;
        } else {
            process.env.SystemRoot = originalSystemRoot;
        }
        if (originalWindir === undefined) {
            delete process.env.WINDIR;
        } else {
            process.env.WINDIR = originalWindir;
        }
    });

    it('uses System32 tar.exe and ignores tar on PATH', () => {
        const [system32Tar, sysnativeTar] = windowsTarCandidates(process.env);
        vi.spyOn(fs, 'existsSync').mockImplementation(candidate => candidate === system32Tar);
        const execFileSync = vi.spyOn(childProcess, 'execFileSync').mockReturnValue(Buffer.from(''));

        extractZipWindows(ARTIFACT, DEST);

        expect(fs.existsSync).toHaveBeenCalledWith(system32Tar);
        expect(fs.existsSync).not.toHaveBeenCalledWith(sysnativeTar);
        expect(execFileSync).toHaveBeenCalledOnce();
        expect(execFileSync.mock.calls[0][0]).toBe(system32Tar);
        expect(execFileSync.mock.calls[0][0]).not.toBe('tar');
        expect(execFileSync.mock.calls[0][1]).toEqual(['-xf', ARTIFACT, '-C', DEST]);
    });

    it('uses Sysnative tar.exe when System32 is redirected', () => {
        const [system32Tar, sysnativeTar] = windowsTarCandidates(process.env);
        vi.spyOn(fs, 'existsSync').mockImplementation(candidate => candidate === sysnativeTar);
        const execFileSync = vi.spyOn(childProcess, 'execFileSync').mockReturnValue(Buffer.from(''));

        extractZipWindows(ARTIFACT, DEST);

        expect(execFileSync.mock.calls[0][0]).toBe(sysnativeTar);
        expect(execFileSync.mock.calls[0][0]).not.toBe(system32Tar);
    });

    it('falls back to Expand-Archive with spaces and apostrophes escaped', () => {
        vi.spyOn(fs, 'existsSync').mockReturnValue(false);
        const execFileSync = vi.spyOn(childProcess, 'execFileSync').mockReturnValue(Buffer.from(''));

        extractZipWindows(ARTIFACT, DEST);

        expect(execFileSync).toHaveBeenCalledWith(
            'powershell.exe',
            [
                '-NoProfile',
                '-NonInteractive',
                '-Command',
                `Expand-Archive -LiteralPath '${quotePowerShellLiteral(ARTIFACT)}' -DestinationPath '${quotePowerShellLiteral(DEST)}' -Force`,
            ],
            { stdio: 'inherit' },
        );
        expect(quotePowerShellLiteral(ARTIFACT)).toContain("O''Brien");
        expect(quotePowerShellLiteral(ARTIFACT)).toContain('pay zip');
    });
});
