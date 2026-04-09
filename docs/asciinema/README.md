# Asciinema Demo

This is the terminal-first version of the lnx ingress walkthrough.

It focuses only on the shell flow:

- `lnx init`
- `lnx clone dev`
- scaffold a Vite app
- run the dev server inside the VM
- `lnx ingress enable`
- `curl http://p5173.dev.lnx/`

## Record

```sh
cd docs/asciinema
./record.sh
```

The cast is written to `docs/asciinema/ingress-demo.cast`.

## Render GIF

```sh
cargo install --locked --git https://github.com/asciinema/agg
cd docs/asciinema
./render-gif.sh
```

The GIF is written to `docs/asciinema/out/ingress-demo.gif`.

## Play In Terminal

```sh
cd docs/asciinema
asciinema play ingress-demo.cast
```

## Notes

`demo.sh` is intentionally scripted rather than recorded live so the asset is reproducible and easy to edit.

`render-gif.sh` uses `agg` to turn the checked-in cast into a GIF. The repository's GitHub Actions workflow uploads that GIF as an artifact instead of checking it into git.
