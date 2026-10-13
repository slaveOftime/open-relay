import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import eslintConfigPrettier from 'eslint-config-prettier'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  // `dist` is the production build, `dev-dist` what vite-plugin-pwa emits while
  // developing (`npx vite`); linting generated output only produces noise like
  // "Definition for rule '@typescript-eslint/ban-types' was not found".
  globalIgnores(['dist', 'dev-dist']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
      eslintConfigPrettier,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
  },
  {
    // Soft size budget (see web/REFACTOR-PLAN.md §2): warns, never fails, so it
    // can sit in the shared config from day one and go quiet as files are
    // split. Two tiers, because a page or a component can carry markup a module
    // cannot, so the same line count means something different in each.
    files: ['src/pages/**/*.tsx', 'src/components/**/*.tsx'],
    rules: {
      'max-lines': ['warn', { max: 500, skipBlankLines: true, skipComments: true }],
    },
  },
  {
    files: ['src/api/**/*.ts', 'src/lib/**/*.ts', 'src/utils/**/*.ts', 'src/hooks/**/*.ts'],
    rules: {
      'max-lines': ['warn', { max: 400, skipBlankLines: true, skipComments: true }],
    },
  },
])
