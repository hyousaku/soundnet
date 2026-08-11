# Third-party notices

SoundNet itself is MIT (see [LICENSE](LICENSE)). This file records what it
depends on and what each of those asks of you.

The distinction that decides almost everything here is **what actually gets
distributed**:

- The **source tree** carries no third-party code. Rust and npm dependencies
  are fetched at build time; `Cargo.lock` and `package-lock.json` name them
  but contain none of their code.
- The **`.deb` built by `packaging/build-deb.sh`** contains exactly one
  program — the engine binary — with the Rust crates it was compiled from
  statically linked in, and the web UI embedded. It does **not** contain
  libroc, ALSA or OpenFEC: those are `Depends:` entries, resolved from the
  target machine's own packages.

So the obligations that actually attach to anything you hand to someone else
are the ones under "Linked into the binary" below. The system libraries are
listed after that for completeness, because `packaging/install.sh` will build
one of them from source on distributions that don't package it.

---

## Linked into the binary (and therefore into any `.deb` you distribute)

| | License | Note |
|---|---|---|
| ~200 Rust crates | MIT / Apache-2.0 (a few `Unlicense OR MIT`, `BSD`) | Permissive. Keep the copyright notices — `cargo about` or `cargo license` will generate the full list if you ever need to ship one. |
| `option-ext` (via `directories`) | **MPL-2.0** | The one weak-copyleft item in the tree. |
| `icu_*`, `idna` data | Unicode-3.0 | Permissive, notice-only. |
| React, React DOM, Zustand, `@xyflow/react` | MIT | Compiled into the embedded web UI. |
| TypeScript, Vite | Apache-2.0 / MIT | Build-time only; no code of theirs ships. |

**MPL-2.0 (`option-ext`)** is file-level copyleft, and it is satisfied here
without doing anything unusual: the crate is used unmodified, and MPL-2.0 §3.2
allows distributing it in executable form inside a larger work under other
terms, provided recipients can obtain the MPL-covered source. Pointing at
<https://crates.io/crates/option-ext> does that. It would only become
interesting if you forked the crate — then your changes to *its* files would
have to stay MPL, while everything else in this repo could remain MIT.

---

## System libraries (dynamically linked, not distributed by this project)

| | License | How it gets there |
|---|---|---|
| **roc-toolkit** (`libroc`) | **MPL-2.0** | apt on Debian/RPi OS trixie; built from upstream source by `install.sh` elsewhere. |
| **ALSA** (`libasound`) | **LGPL-2.1+** | apt, everywhere. |
| **OpenFEC** | **CeCILL-C** (plus one file CC-BY-SA-3.0) | Pulled in by roc's `--build-3rdparty=openfec`, which is how `install.sh` builds roc when the distribution has no `libroc0.4`. |

None of these are copied into anything this repo produces, so no notice or
source-offer obligation falls on you as things stand. Two situations would
change that:

1. **Bundling libroc into the `.deb`** (static linking, or shipping the `.so`
   to avoid the source build on Ubuntu). You would then be distributing
   MPL-2.0 and CeCILL-C code, and would need to carry their notices and make
   their source available. CeCILL-C is the more awkward of the two — it is a
   French license with its own text; it is LGPL-compatible in spirit but not
   identical, and the FSF considers it a free software license.
2. **Shipping a disk image** with everything preinstalled — same thing, plus
   ALSA's LGPL, which additionally requires that recipients be able to relink
   against a modified libasound. Dynamic linking already satisfies that; a
   static one would not.

---

## `@xyflow/react` attribution

The patch bay shows React Flow's attribution badge, and should keep doing so.

The library's license is plain MIT and does not require it — MIT asks only
that the copyright notice travel with copies of the software, not that
anything appear on screen. Removing the badge would have been legally fine,
and `proOptions={{ hideAttribution: true }}` did exactly that for a while.
But the xyflow project asks that it stay unless you hold a Pro subscription,
and that request is how the library is funded. Since we are not paying for it,
leaving the badge up is the least we can do.

If you ever do want it gone, buy the subscription rather than setting the
flag.

---

*This is a practical inventory compiled from the license metadata in
`Cargo.lock`, `package-lock.json` and the Debian copyright files on the
deployment machines — not legal advice.*
