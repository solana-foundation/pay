#!/usr/bin/env node

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const packageManifestPath = resolve(
    repositoryRoot,
    'typescript/packages/solana-pay/core/package.json',
);
const releaseWorkflowPath = resolve(repositoryRoot, '.github/workflows/release-cli.yml');
const binaryName = 'pay';

function artifactName(target) {
    const extension = target.endsWith('-pc-windows-msvc') ? 'zip' : 'tar.gz';
    return `${binaryName}-${target}.${extension}`;
}

function uniqueSorted(values) {
    return [...new Set(values)].sort();
}

function setDifference(left, right) {
    return left.filter((value) => !right.includes(value));
}

function releaseTargets(workflow) {
    const targets = [...workflow.matchAll(/^\s*- target:\s*([^\s#]+)\s*$/gm)].map(
        ([, target]) => target,
    );

    if (targets.length === 0) {
        throw new Error(
            `No release targets found in ${releaseWorkflowPath}. Keep the build matrix target entries on their own lines.`,
        );
    }

    if (new Set(targets).size !== targets.length) {
        throw new Error(`Duplicate release targets found in ${releaseWorkflowPath}.`);
    }

    return uniqueSorted(targets);
}

function declaredArtifacts(manifest) {
    if (!manifest.supportedPlatforms || typeof manifest.supportedPlatforms !== 'object') {
        throw new Error(`${packageManifestPath} must define supportedPlatforms.`);
    }

    return uniqueSorted(
        Object.entries(manifest.supportedPlatforms).map(([target, metadata]) => {
            if (!metadata || typeof metadata.artifact !== 'string') {
                throw new Error(`supportedPlatforms.${target} must define an artifact.`);
            }

            const expected = artifactName(target);
            if (metadata.artifact !== expected) {
                throw new Error(
                    `supportedPlatforms.${target}.artifact is ${metadata.artifact}; expected ${expected}.`,
                );
            }

            return metadata.artifact;
        }),
    );
}

function printDifference(label, values) {
    if (values.length === 0) {
        return;
    }

    console.error(`${label}:`);
    for (const value of values) {
        console.error(`  - ${value}`);
    }
}

function main() {
    const packageManifest = JSON.parse(readFileSync(packageManifestPath, 'utf8'));
    const declared = declaredArtifacts(packageManifest);
    const produced = releaseTargets(readFileSync(releaseWorkflowPath, 'utf8')).map(artifactName);
    const onlyDeclared = setDifference(declared, produced);
    const onlyProduced = setDifference(produced, declared);

    if (onlyDeclared.length > 0 || onlyProduced.length > 0) {
        console.error('Native release artifact contract failed.');
        console.error(`npm manifest: ${packageManifestPath}`);
        console.error(`release matrix: ${releaseWorkflowPath}`);
        printDifference('Declared by npm but not produced by release', onlyDeclared);
        printDifference('Produced by release but not declared by npm', onlyProduced);
        process.exitCode = 1;
        return;
    }

    console.log(`Native release artifact contract passed for ${declared.length} targets.`);
}

main();
