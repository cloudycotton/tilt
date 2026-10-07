# tilt-live

See and control any Linux desktop, right in your browser.

```sh
npx tilt-live
```

Open the link it prints, click the cursor button, and the desktop is yours.

- `npx tilt-live --display :1`: every flag goes to [tilt](https://github.com/cloudycotton/tilt/blob/main/docs/guide.md#flags).
- Always runs the latest release (checksum-verified). Pin one with `TILT_VERSION=0.2.0`, skip the check with `TILT_NO_UPDATE=1`.
- Keeps its token in `~/.config/tilt-live/token` unless you pass `--token`, `--token-file` or `--no-auth`.
- Linux on x86_64 or arm64, Node 18+. MIT licensed.
