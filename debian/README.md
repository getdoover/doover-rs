# Debian builds for the Rust CLI

Publishes `doover-grpc` to `apt.u.doover.com`, mirroring pydoover's
`debian/` + `push_to_apt.yml` setup.

## What ships

| Path | What it is |
| --- | --- |
| `/usr/bin/doover-grpc` | the CLI, under its own name |
| `/usr/bin/pydoover` | symlink to the above |

The symlink is why existing install flows don't need to change: `pydoover
device_agent ...` keeps working, and clap derives its usage line from `argv[0]`,
so the help text still says `pydoover`.

Note `/usr/bin/doover` is deliberately NOT ours -- that path belongs to the
unrelated `doover-cli` package.

## The pydoover split (coordination required)

`doover-pydoover` <= 1.10 shipped **both** the Python library and the CLI:

    /usr/lib/python3/dist-packages/pydoover   <- library, apps `import` this
    /usr/bin/pydoover                         <- CLI

This package only replaces the **CLI**. The library must keep being published
from the pydoover repo, or every device that upgrades loses `import pydoover`.

Two changes have to land together, or dpkg will refuse to unpack:

1. **here**: `doover-grpc` declares `Breaks`/`Replaces: doover-pydoover (<< 1.11)`
   and installs `/usr/bin/pydoover`.
2. **pydoover 1.11.0**: ships only the library (`[project.scripts]` dropped, so
   `/usr/bin/pydoover` is no longer in the package) and declares
   `Depends: doover-grpc` so upgrades pull the Rust CLI in.

Until (2) ships, `apt install doover-grpc` will want to remove or break
`doover-pydoover` on a device rather than upgrade it. The `<< 1.11` bound is the
contract between the two packages -- if pydoover's release version changes, that
bound must change here to match.

Verified with `dpkg --compare-versions`: 1.1.4 / 1.9.4 / 1.10.0 all fall inside
`<< 1.11` (so the handover applies), while 1.11.0 falls outside it (so the Breaks
never fires against the new package).

Note pydoover 1.11.0 also drops the `pydoover` console script from the **PyPI
wheel**, not just the deb -- `pip install pydoover` no longer provides the
command. The `pydoover.cli` module remains importable.

## Architectures

Unlike pydoover (`Architecture: all` -- one package for everything, since the
interpreter absorbs the difference), a Rust binary is per-architecture. We build
`arm64`, `amd64` and `armhf`, and apt resolves the right one per host.

## Building

`debian/rules` packages a **prebuilt** binary from
`debian/prebuilt/$DEB_HOST_ARCH/doover`; it does not run cargo. Produce it with
the multi-arch Dockerfile first (one native rustc cross-compiles every arch via
cargo-zigbuild -- no QEMU, and the apt CI image needs no Rust toolchain):

    docker buildx build --platform linux/arm64 --target bin \
      --output type=local,dest=debian/prebuilt/arm64 .
    dpkg-buildpackage -a arm64 -b -us -uc

CI does both steps per arch; see `.github/workflows/push_to_apt.yml`.
