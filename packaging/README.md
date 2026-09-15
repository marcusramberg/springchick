# Distro packages

Binary packages for postmarketOS/Alpine (`.apk`), Mobian/Debian (`.deb`) and
Arch (`.pkg.tar.zst`), hosted in the Forgejo package registries on
code.bas.es. NixOS is not packaged here — it has the flake.

Alpine gets a second package, `postmarketos-ui-springchick`: the apk equivalent
of `nix/module.nix` (session dependencies + the wvkbd user unit), built from
`packaging/nfpm-ui.yaml` by `package.sh`. Other distros do not have it — the
equivalent wiring there is a differently-named package with a different
dependency set.

```bash
packaging/build.sh debian arm64        # -> dist/springchick_0.1.0-1_arm64.deb
FORGEJO_TOKEN=... packaging/publish.sh # uploads everything in dist/
```

`just pkg debian arm64` / `just pkg-publish` do the same.

## Nightly (Woodpecker)

`.woodpecker/nightly.yaml` builds all three distros × both arches and uploads
to separate nightly repos (`nightly` component/repository, `springchick-nightly`
Arch group). Steps run in the distro image directly — no podman — calling the
same `compile.sh` and `package.sh` this script does.

Two things must exist on the Woodpecker side:

- a cron job named **nightly** in the repo settings (the `cron:` filter matches
  on that name),
- a **`forgejo_token`** secret with package write scope.

It also needs one agent per architecture; `labels: platform: linux/<arch>`
selects them. Without an amd64 agent, drop those matrix entries.

Versions are date-stamped (`0.1.0.20260913-1`) because the registries reject
re-uploading an existing filename. Dots only: Arch `pkgver` rejects `-` and `~`.

CI does not cache cargo between runs, so a nightly pays the full build —
including Skia from source on Alpine.

## How it works

`build.sh` runs `compile.sh` inside a container of the target distro — the
binary has to link against that distro's glibc/musl — then `package.sh` turns
the staged binaries into a package with
[nfpm](https://nfpm.goreleaser.com/): one `nfpm.yaml` produces all three
formats, with per-packager dependency lists. All three ship the same payload,
systemd user units included — postmarketOS is systemd now, and the
`Type=notify` unit is how `springchick-session` raises
`graphical-session.target`.

Foreign-arch builds go through qemu-user (`--platform`), which is slow and
needs a binfmt handler registered with the `F` flag — NixOS's
`boot.binfmt.emulatedSystems` registers `P` only, which containers cannot use.
Register one once per boot:

```bash
podman run --privileged --rm docker.io/tonistiigi/binfmt --install amd64
```

On a native runner of the target arch none of this applies.

Cargo's registry and target dir live in named container volumes
(`springchick-<distro>-<arch>-{cargo,target}`), so only the first build of each
combination pays full price. `podman volume rm` them to start clean.

## Skia

`skia-bindings` downloads a prebuilt Skia for glibc targets. **There is no
musl prebuilt**, so the Alpine build falls through to a full source build —
skia-bindings fetches the Skia tree itself, but needs `gn`, `samurai` and
clang from the distro (the bundled gn/ninja are glibc binaries). Slow
(~40 min), cached in the target volume afterwards.

To skip that, point every build at a blob you host yourself:

```bash
SKIA_BINARIES_URL=https://code.bas.es/api/packages/marcus/generic/skia/0.99.0/skia-musl.tar.gz \
  packaging/build.sh alpine arm64
```

Bumping `skia-safe` invalidates the blob; `nix/package.nix` has the naming
scheme for the prebuilt archives.

## Consuming the registries

```bash
# Alpine / postmarketOS
curl -JO https://code.bas.es/api/packages/marcus/alpine/key   # -> /etc/apk/keys/
echo "https://code.bas.es/api/packages/marcus/alpine/edge/main" >> /etc/apk/repositories

# Debian / Mobian
curl https://code.bas.es/api/packages/marcus/debian/repository.key \
  -o /etc/apt/keyrings/forgejo-marcus.asc
echo "deb [signed-by=/etc/apt/keyrings/forgejo-marcus.asc] https://code.bas.es/api/packages/marcus/debian trixie main" \
  > /etc/apt/sources.list.d/springchick.list

# Arch
# /etc/pacman.conf:
#   [springchick]
#   Server = https://code.bas.es/api/packages/marcus/arch/springchick/$arch
```

## Not done here

No native source recipes (APKBUILD / `debian/` / PKGBUILD). These are binary
packages built from this tree; upstream inclusion in aports or Debian would
need real recipes and a vendored-source tarball (the `smithay` git dependency
means `cargo build` needs network otherwise). `postmarketos-ui-springchick`
borrows the postmarketOS UI naming convention but cannot carry the
`pmbootstrap` half of it (`_pmb_groups`, `_pmb_recommends`, listing in
`pmbootstrap init`) — that metadata is read from the pmaports tree, not from a
compiled package.
