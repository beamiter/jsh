# Release manifest signing

Release installs trust a **detached minisign signature** over `manifest.json`,
verified against a public key pinned in `scripts/install-jsh.sh`. The
same-origin `.tar.gz.sha256` sidecar remains mandatory and must equal the
digest named in the signed manifest.

## Key material

| File | Role |
|------|------|
| `keys/jsh-minisign.pub` | Public key (committed; also embedded in the installer) |
| `keys/jsh-minisign.secret` | Secret key (**never** commit; gitignored) |

Generate (empty password, for non-interactive CI signing):

```sh
mkdir -p keys
printf '\n\n' | minisign -G -p keys/jsh-minisign.pub -s keys/jsh-minisign.secret
```

After rotating keys, update the `JSH_MINISIGN_PUBKEY_PINNED` constant in
`scripts/install-jsh.sh` to the new `R…` line from the `.pub` file.

## GitHub Actions secret

Set repository secret **`JSH_MINISIGN_SECRET_KEY`** to the **full contents** of
`keys/jsh-minisign.secret` (both lines: the untrusted comment and the key).

The release workflow writes that secret to a temporary file, runs
`minisign -Sm manifest.json`, and uploads `manifest.json.minisig` with the
release assets. An empty secret fails the job so an unsigned manifest is never
published.

## Installer override (fixtures only)

`JSH_MINISIGN_PUBKEY` is honoured **only when** `JSH_INSTALL_BASE_URL` is also
set. Production `curl | sh` installs always use the pinned constant.
