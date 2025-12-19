/**
 * Commit message convention for this repo:
 *   <category>: <subject>
 *
 * Categories are lower-case and may include slashes/hyphens/spaces and optional
 * scopes in parentheses (e.g. `docs(spec): ...`, `migrate/runtime: ...`).
 */
module.exports = {
  parserPreset: {
    parserOpts: {
      headerPattern: /^([a-z][a-z0-9/()! \\-]*): (.+)$/,
      headerCorrespondence: ["type", "subject"],
    },
  },
  rules: {
    "type-empty": [2, "never"],
    "type-case": [2, "always", "lower-case"],
    "subject-empty": [2, "never"],
    // This repo has historically mixed subject casing; keep this permissive.
    "subject-case": [0],
    "subject-full-stop": [0],
    "header-max-length": [2, "always", 100],
  },
};

