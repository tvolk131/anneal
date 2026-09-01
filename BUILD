# The dogfood: anneal builds and unit-tests itself. The package is the
# repository root (the cargo workspace root); everything that is not a build
# input is excluded so editing it leaves action identity unchanged — a docs
# PR is a cache hit wherever the store persists (CI restores .anneal/store).
cargo_workspace(
    name = "ws",
    exclude = [
        ".DS_Store",
        ".claude",
        ".github",
        ".gitignore",
        "BUILD",
        "DESIGN.md",
        "TODO.md",
        "docker",
        "docs",
        "flake.lock",
        "flake.nix",
        "README.md",
        "scripts",
        "spikes",
    ],
)
