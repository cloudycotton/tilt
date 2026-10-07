# tilt-live

Extremely fast live view and control of any Linux desktop, right in your browser.

```sh
npx tilt-live                 # streams $DISPLAY (or :0) on port 6090 and prints the link to open
npx tilt-live --display :1    # every flag goes to tilt
```

Each start runs the latest [tilt](https://github.com/cloudycotton/tilt) release, downloading it
(checksum-verified) when a newer one is out. `TILT_VERSION=0.2.0` pins a release and
`TILT_NO_UPDATE=1` skips the check. Without `--token`, `--token-file` or `--no-auth`, a token is
kept in `~/.config/tilt-live/token`. Linux on x86_64 or arm64, Node 18+. MIT licensed.
