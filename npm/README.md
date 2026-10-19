# tilt-live

See and control any Linux desktop, right in your browser.

```sh
npx tilt-live
```

Run it on the Linux machine or VM whose screen you want, open the link it prints, click the cursor
button, and the desktop is yours.

- The first run downloads [tilt](https://github.com/cloudycotton/tilt) (checksum-verified). After
  that tilt updates itself whenever it starts; `TILT_NO_UPDATE=1` turns this off.
- `npx tilt-live --display :1`: every flag goes to [tilt](https://github.com/cloudycotton/tilt/blob/main/docs/guide.md#flags).
- Linux on x86_64 or arm64, Node 18+. MIT licensed.
