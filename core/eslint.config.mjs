import config from '@solana/eslint-config-solana';

export default [
    ...config,
    {
        rules: {
            '@typescript-eslint/no-explicit-any': 'off',
            '@typescript-eslint/no-redundant-type-constituents': 'off',
            '@typescript-eslint/no-unsafe-enum-comparison': 'off',
            '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', caughtErrorsIgnorePattern: '^_', destructuredArrayIgnorePattern: '^_' }],
            'sort-keys-fix/sort-keys-fix': 'off',
            'typescript-sort-keys/interface': 'off',
        },
    },
    {
        files: ['example/**/*', 'test/**/*'],
        rules: {
            '@typescript-eslint/no-floating-promises': 'off',
            '@typescript-eslint/no-non-null-assertion': 'off',
        },
    },
];
