# springchick — iOS Springboard-style Wayland compositor
#
# Run `just` for this list. All recipes assume you're inside `nix develop`.

# -- Build -----------------------------------------------------------

build:
	cargo build -p sc-compositor

release:
	cargo build -p sc-compositor --release

# -- Check -----------------------------------------------------------

# Fast compile check (no linking)
check:
	cargo check --tests

# Lint
clippy:
	cargo clippy --workspace -- -D warnings

# -- Test ------------------------------------------------------------

test:
	cargo test --workspace

# Run tests for a single crate (e.g. `just test-crate sc-layout`)
test-crate crate:
	cargo test -p {{crate}}

# Run tests for a specific module (e.g. `just test-module sc-compositor ui_state::`)
test-module crate module:
	cargo test -p {{crate}} {{module}}

# -- Docs ------------------------------------------------------------

doc:
	cargo doc --workspace --no-deps

doc-open:
	cargo doc --workspace --no-deps --open

# -- Coverage --------------------------------------------------------

# Requires llvm-tools-preview extension (included in devshell)
coverage:
	cargo llvm-cov --workspace --lcov --output-path lcov.info

coverage-html:
	cargo llvm-cov --workspace --html

# -- VM checks (headless, real DRM path) -----------------------------

# Nix system to build checks for (default: detected from uname).
# Always build the check matching your host arch — cross-building under
# qemu-user emulation crashes rustc.
# Examples: aarch64-linux, x86_64-linux
# Nix system to build checks for. Override with `NIX_SYSTEM=x86_64-linux just vm-boot`.
# Defaults to a guess based on uname -m; always build the check matching your host arch.
export NIX_SYSTEM := `python3 -c 'import platform;print(platform.machine()+"-linux")'`

vm-boot:
	nix build .#checks.{{NIX_SYSTEM}}.vm-boot -L

vm-switcher:
	nix build .#checks.{{NIX_SYSTEM}}.vm-switcher -L

vm-dialog:
	nix build .#checks.{{NIX_SYSTEM}}.vm-dialog -L

vm-rotation:
	nix build .#checks.{{NIX_SYSTEM}}.vm-rotation -L

vm-arrange:
	nix build .#checks.{{NIX_SYSTEM}}.vm-arrange -L

vm-lock:
	nix build .#checks.{{NIX_SYSTEM}}.vm-lock -L

vm-capture:
	nix build .#checks.{{NIX_SYSTEM}}.vm-capture -L

vm-portal:
	nix build .#checks.{{NIX_SYSTEM}}.vm-portal -L

# Run all VM checks
vm-all:
	for check in vm-boot vm-switcher vm-dialog vm-rotation vm-arrange vm-lock vm-capture vm-portal; do \
		just "$$check" || exit 1; \
	done

# -- Misc ------------------------------------------------------------

# Warm up the devshell (builds nothing, just enters the environment)
shell:
	nix develop --command true
