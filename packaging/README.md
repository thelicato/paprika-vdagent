`packaging/` contains distro-specific packaging inputs, not project-specific platform limits.

Current layout:

- `packaging/pacman/`: pacman `PKGBUILD` recipe for Arch Linux and related pacman-based environments
- `packaging/systemd/user/`: shared user service unit installed by all package formats

`.deb` and `.rpm` are generated from `Cargo.toml` metadata instead of separate handwritten spec trees. That keeps the package manifest close to the Rust crate metadata and avoids duplicating package version, description, and asset lists in multiple places.
