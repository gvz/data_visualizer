# Bundling Python for the Windows release

The Windows portable zip ships a self-contained CPython with numba/numpy so end
users install nothing. The Linux `.deb` instead depends on `python3-numba`
(resolved by apt) and bundles nothing.

## Steps (run on Windows CI, matching the release toolchain)

1. Download a relocatable CPython from python-build-standalone
   (`cpython-3.x.y+*-x86_64-pc-windows-msvc-*.tar.zst`) and extract it to
   `dist/python/`.
2. Pre-install the numeric stack into that interpreter:
   ```
   dist/python/python.exe -m pip install --no-warn-script-location numba numpy
   ```
3. Build datavis against that interpreter so PyO3 links its `pythonXY.dll`:
   ```
   set PYO3_PYTHON=%CD%\dist\python\python.exe
   cargo build --release --features scripting
   ```
4. Assemble the zip: `datavis.exe`, the matching `pythonXY.dll` beside it, and
   the `python/` tree. At runtime `main.rs` sets `PYTHONHOME` to `python/`.

Keep the interpreter version pinned in CI. Pin the numba/numpy wheels by hash
(a `requirements.txt` with hashes) so the bundle is reproducible — cargo-vet
does not cover Python wheels.
