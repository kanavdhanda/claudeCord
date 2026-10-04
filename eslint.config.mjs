import js from "@eslint/js";
import globals from "globals";
import tseslint from "typescript-eslint";
import { defineConfig } from "eslint/config";

export default defineConfig(
  { ignores: ["**/dist/**", "**/.tsc/**", "coverage/**", "**/*.d.ts", "packages/node/test/fixtures/**"] },
  js.configs.recommended,
  tseslint.configs.recommended,
  {
    languageOptions: { globals: { ...globals.node } },
    rules: {
      "@typescript-eslint/no-unused-vars": [
        "error",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_", caughtErrors: "none" },
      ],
      "@typescript-eslint/no-explicit-any": "error",
      eqeqeq: ["error", "always"],
      "prefer-const": "error",
      // The redactor and the terminal sanitiser match control characters on purpose.
      "no-control-regex": "off",
    },
  },
  {
    // k6 provides these globals to its scripts.
    files: ["packages/hub/bench/k6/**/*.js"],
    languageOptions: { globals: { __ENV: "readonly", __VU: "readonly", open: "readonly" } },
  },
  {
    // Promises that nobody awaits are how errors get swallowed, so source files are checked with type information.
    files: ["packages/*/src/**/*.ts"],
    languageOptions: { parserOptions: { projectService: true, tsconfigRootDir: import.meta.dirname } },
    rules: {
      "@typescript-eslint/no-floating-promises": "error",
      "@typescript-eslint/no-misused-promises": ["error", { checksVoidReturn: false }],
    },
  },
);
