# Digital Works file format notes

- `DWM-FORMAT.md` — the `.dwm` / `.dwt` / `.map` format, recovered by reading
  the Digital Works 3.0.5 executable as data (nothing was executed) and
  checked against every file of the official package.
- `coverage.md` — class and field statistics over the package's 503 files,
  the mapping to the bitwerk model and what bitwerk does with each item.

`tools/dwm_parse.py` is the Python reference parser used for that research
(standard library only; run it with `python3 -I tools/dwm_parse.py FILE_OR_DIR…`
on files you trust to be data — it never executes anything from them). It
parses all 503 files of the package; the Rust importer
(`crates/bw-io/src/dwm/`) follows the same layouts and limits and is the one
the application uses.
