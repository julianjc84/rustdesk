# Bare-metal build fixes

This fork builds RustDesk **on a normal Linux desktop** — no Docker, no GitHub Actions.
Upstream doesn't support that path: several steps that are required to produce a working
binary live in CI (`.github/workflows/bridge.yml`, `flutter-build.yml`) or in the Docker
recipe, not in the sources. **A stock checkout therefore does not build on bare metal.**

The commit carrying this file closes those gaps. Nothing here changes how RustDesk
*behaves* — it only makes `python3 build.py --flutter` work outside CI.

Full environment reference (per-distro packages, toolchain pins, every cliff and its
symptom): `RD_JulianFiles/build_environment.md`.

---

## `build.py` — run the flutter_rust_bridge codegen

Upstream's `build.py` never runs codegen. CI does it in a separate job, so on bare metal
the four generated files simply never appear and the Dart build fails on missing symbols:

    src/bridge_generated.rs   src/bridge_generated.io.rs
    flutter/lib/generated_bridge.dart   flutter/lib/generated_bridge.freezed.dart

They're gitignored, so this recurs after every `flutter clean`. `run_frb_codegen()` now
runs it as part of the build.

**`_detect_clang_resource_include()` is the part that matters.** ffigen shells out to
libclang, which cannot find `stdbool.h` unless it is handed clang's own resource include
directory. Without it codegen *appears* to succeed but writes a poisoned line into
`generated_bridge.dart`:

```dart
typedef bool = ffi.NativeFunction<ffi.Int Function(ffi.Pointer<ffi.Int>)>;
```

That shadows Dart's built-in `bool`, and the build dies much later with dozens of
misleading errors in `peer_card.dart`, `remote_input.dart` and friends. The fix detects
the path with `clang -print-resource-dir` and passes it as `--llvm-compiler-opts`;
`_verify_bridge_output()` then hard-fails if the poisoned typedef ever reappears, so this
can't silently regress.

`_detect_llvm_path()` and `_detect_vcpkg_root()` do the same job for `LIBCLANG_PATH` and
`VCPKG_ROOT`, which CI sets as environment variables and a local shell usually hasn't.

`_resolve_frb_codegen()` / `_ensure_frb_codegen_installed()` check that the installed
`flutter_rust_bridge_codegen` matches `FRB_VERSION_PIN` (1.80, pinned by `Cargo.toml` and
`pubspec.yaml`) and print the exact `cargo install` line when it doesn't. A mismatched
codegen produces a bridge that compiles but misbehaves at runtime.

## `build.py` — `--clean` chaining, pub deps, log ordering

`--clean` used to be a standalone action that exited before building. It now chains, so
`python3 build.py --clean --flutter` cleans, restores pub dependencies and builds in one
pass — the sequence you actually want after a toolchain change. Also restores the
`flutter pub get` that a clean wipes out, and fixes log ordering so failures appear next
to the step that caused them.

## `.gitignore` — makepkg artifacts

Building the Arch/Manjaro package runs `makepkg` in-tree, which drops `res/pkg/` and
`res/*.pkg.tar.zst` into the working copy. Upstream never sees these because CI packages
elsewhere. Ignored so `git status` stays readable.

## `flutter/pubspec.lock` — `vector_math`

`pubspec.yaml` declares `vector_math: ^2.1.4` directly, but the committed lock still marks
it `transitive` — upstream promoted it and never committed the regenerated lock. Metadata
only; it changes no resolved version. But every `flutter pub get` rewrites that line, so
on a persistent checkout the lockfile shows up dirty after each build. CI never notices
because the runner is destroyed before anything inspects the tree.

---

## Not on this branch

The window_manager SIGSEGV-at-close fork pin was **dropped** — upstream fixed it
themselves in `rustdesk-org/window_manager#8` (merged 2026-07-29), and 1.4.9 pins a
plugin that contains it. Do not re-add it.

## Also on this branch, but not build fixes

Two commits below this one change runtime behaviour and are unrelated to bare metal —
they'd be equally valid in a CI build:

- `fix(linux): remove hourly server restart that destabilizes Cinnamon`
- `fix(linux): add service mode installation for non-Debian distros`
