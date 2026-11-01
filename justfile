wasm := "target/wasm32-wasip2/release/component_scrape.wasm"
# OCI reference to publish to (registry/namespace/name, no tag). Override with OCI_REF.
component_ref := env("OCI_REF", "actpkg.dev/library/scrape")

act := env("ACT", "npx @actcore/act")
actbuild := env("ACT_BUILD", "npx @actcore/act-build")

# Fetch the WIT deps, and check the one thing this component cannot build
# without: the engine compiles C along the way (SpiderMonkey, FreeType,
# aws-lc-rs, swgl), so it needs wasi-sdk at /opt/wasi-sdk.
init:
    #!/usr/bin/env bash
    set -euo pipefail
    WKG_CONFIG_FILE=wkg-registry.toml wkg wit fetch --type wit
    test -d /opt/wasi-sdk || {
        echo "wasi-sdk not found at /opt/wasi-sdk." >&2
        echo "The engine builds C along the way: SpiderMonkey, FreeType," >&2
        echo "aws-lc-rs, swgl. Get it from" >&2
        echo "https://github.com/WebAssembly/wasi-sdk/releases" >&2
        exit 1
    }

setup: init
    prek install

# Build and pack. Packing is part of building on purpose: `cargo build` alone
# produces a wasm with no `act:component` section, which declares no capability
# ceiling, so at runtime every grant is refused as "outside ceiling" and the
# failure points anywhere but at the missing metadata.
build: init
    cargo build --target wasm32-wasip2 --release
    {{actbuild}} pack {{wasm}}

# Re-embed act:component metadata and act:skill without rebuilding. `pack` is
# idempotent, so running it after `build` is harmless.
pack:
    {{actbuild}} pack {{wasm}}

# Fast host-target tests of the conversion pipeline. The engine cannot compile
# on the host, which is exactly why conversion lives in its own module.
test-unit:
    cargo test --target x86_64-unknown-linux-gnu

test: build
    # Serial on purpose. Each test drives its own `act run`, and this component
    # carries the same fleet-heavyweight engine servo does: ~3 GB peak RSS per
    # instance against a ~120 MB module. In parallel, the overlapping compiles
    # and instantiations starve the runner — one at a time, as servo does it.
    cd e2e && ACT="{{act}}" WASM="../{{wasm}}" cargo test -- --test-threads=1

publish: build
    #!/usr/bin/env bash
    set -euo pipefail
    INFO=$({{act}} inspect component-manifest {{wasm}})
    VERSION=$(echo "$INFO" | jq -r .std.version)
    {{actbuild}} push {{wasm}} "{{component_ref}}:$VERSION" --skip-if-exists --also-tag latest
