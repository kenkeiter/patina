# Crate audit notes

One section per third-party dependency: version, why we need it, `no_std`
status, and its unsafe surface. Versions are what `Cargo.lock` resolves to;
the manifest pins the major version.

## bitflags

- **Version:** 2.13.x (`bitflags = { version = "2", default-features = false }`)
- **Used by:** `patina-abi`
- **Why:** type-safe flag sets for Linux ABI flag words (`OpenFlags`,
  `MapFlags`, `ProtFlags`, `CloneFlags`, `SaFlags`, `EpollFlags`,
  `MsgFlags`) with `from_bits_retain` for user-supplied values that may carry
  unknown bits.
- **no_std:** yes, unconditionally; no default features are enabled (no
  `serde`/`arbitrary`/`std`).
- **Unsafe surface:** none in the generated code we use; the macro expands to
  plain integer newtypes. No proc-macro (it is a `macro_rules!` crate), so no
  build-time code execution.

## zerocopy (+ zerocopy-derive)

- **Version:** 0.8.x (`zerocopy = { version = "0.8", default-features = false, features = ["derive"] }`)
- **Used by:** `patina-abi`
- **Why:** byte-safe conversion of `#[repr(C)]` ABI structs to and from user
  memory (`IntoBytes::as_bytes`, `FromBytes::read_from_bytes`) without
  hand-written `unsafe` transmutes. The `IntoBytes` derive also refuses
  structs with implicit padding, which forces every ABI struct to spell out
  its padding fields — a useful guard against layout mistakes (the layout
  tests are the primary guard).
- **no_std:** yes; `default-features = false` drops `alloc`/`std`
  integration.
- **Unsafe surface:** zerocopy is built on `unsafe` internally (pointer casts
  justified by its trait invariants) and the derives emit `unsafe impl`s
  after checking layout at compile time. It is maintained by Google, heavily
  fuzzed, and formally reasoned about (Kani proofs in-tree); it is the
  standard choice for this job. `patina-abi` itself is
  `#![deny(unsafe_code)]`. `zerocopy-derive` is a proc-macro (runs at build
  time on the host) and pulls in `syn`/`quote`/`proc-macro2`, which are
  build-only dependencies and are not linked into the kernel.
