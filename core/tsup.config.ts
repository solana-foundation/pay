import { defineConfig } from 'tsup';

export default defineConfig({
    entry: ['src/index.ts'],
    format: ['esm', 'cjs'],
    dts: false,
    sourcemap: true,
    clean: true,
    outDir: 'dist',
    target: 'es2022',
    splitting: false,
    onSuccess: 'tsc --emitDeclarationOnly',
});
