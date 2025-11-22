## Current gaps and UX notes

- Tool installs now bind to px-managed runtimes by default and no longer print duplicate resolver lines.
- Previously reported issues fixed: `px add` leaves projects consistent without an extra `px sync`, runtime resolution prefers px-managed interpreters, `px run` honors `[project.scripts]` console entries (sampleproject works), and migrate no longer crashes on missing `[project].name`. Dev-only migrations now materialize dev deps (psf/requests succeeds and `px run pytest` works).
