import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["packages/*/test/**/*.test.ts", "scripts/**/*.test.mjs"],
    testTimeout: 10_000,
    coverage: {
      provider: "v8",
      include: ["packages/*/src/**/*.ts"],
      // Process entry points only parse argv and start things. They run in the end-to-end tests, but as child
      // processes, which this coverage run cannot see.
      exclude: [
        "**/*.d.ts",
        "packages/hub/src/index.ts",
        "packages/node/src/cli.ts",
        "packages/agent-tools/src/mcp.ts",
      ],
      reporter: ["text-summary", "json-summary", "lcov"],
      reportsDirectory: "coverage",
      // A few points under what is measured today, so a real regression fails and normal variation does not.
      // Raise these as coverage improves. Never lower them to make a change pass.
      thresholds: { statements: 88, branches: 78, functions: 86, lines: 90 },
    },
  },
});
