# macOS CLI for agents

The macOS app includes `Contents/MacOS/popsicle`. Install a stable command with
`bash macos/install-cli.sh /Applications/Popsicle.app` (or another app path).
Use `$HOME/.local/bin/popsicle` directly if that directory is not on PATH.

Commands emit newline-delimited JSON with `schema_version: 1`; help and version
are plain text. Exit status is 0 for success, 1 for operational failures, and 2
for invalid arguments. Never infer successful flashing from progress alone:
require exit 0 and the final `complete` event with `verified: true`.

```sh
popsicle list
popsicle inspect image.img.zst
popsicle flash image.img.zst --device /dev/disk7 --token 'TOKEN_FROM_LIST' --yes --dry-run
popsicle flash image.img.zst --device /dev/disk7 --token 'TOKEN_FROM_LIST' --yes
```

Select the user's intended device using its label and capacity; never assume
the example disk number is correct. Flashing erases that disk. A token identifies
the current attachment and prevents blindly reusing a disk number after a swap.
The token is an identity check, not an authorization credential. `--dry-run`
validates the image, attachment, and capacity without unmounting or authorization.
Raw ISO/IMG and Zstandard streams are detected by contents; decompression is
streaming, with a validation pass before disk access and mandatory readback verification.

Run as the normal logged-in user, not through sudo. macOS requests administrator
approval attributed to Popsicle for the exact removable disk. Human approval may
be needed, so unattended/headless first-time authorization is not guaranteed.
Only external physical USB/SD whole disks are eligible. Popsicle exposes no
general privileged shell, persistent root service, or raw-descriptor API. An
agent's own execution sandbox permissions are separate and still apply.

While running, `authorizing`, `started`, `progress`, `device_error`, and `complete`
events describe status. Any nonzero exit, interruption, missing completion event,
or `device_error` means the drive must be treated as incomplete. Retrying requires
reselecting the intended device, and rewrites from the start. Do not automatically
retry a destructive operation without the user's authorization.
