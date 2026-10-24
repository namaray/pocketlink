# Pocketlink tasks. `just install` puts everything in ~/.local.

prefix := env_var('HOME') / ".local"
bindir := prefix / "bin"
datadir := prefix / "share"
autostart := env_var_or_default('XDG_CONFIG_HOME', env_var('HOME') / ".config") / "autostart"

# Build release binaries.
build:
    cargo build --release -p pocketlink -p pocketlink-daemon

# Run the app (starts the daemon on demand once installed).
run:
    cargo run -p pocketlink

# Run the daemon in the foreground with debug logs.
daemon:
    RUST_LOG=info,pocketlink_proto=debug,pocketlink_daemon=debug cargo run -p pocketlink-daemon

# Unit tests, plus the end-to-end test against a real daemon.
test:
    cargo test --workspace
    cargo test -p pocketlink-daemon --test e2e -- --ignored

# Install the app, the daemon, D-Bus activation and autostart into ~/.local.
install: build
    install -Dm755 target/release/pocketlink {{bindir}}/pocketlink
    install -Dm755 target/release/pocketlink-daemon {{bindir}}/pocketlink-daemon
    install -Dm644 data/io.github.namaray.Pocketlink.desktop {{datadir}}/applications/io.github.namaray.Pocketlink.desktop
    install -Dm644 crates/pocketlink/resources/icon.svg {{datadir}}/icons/hicolor/scalable/apps/io.github.namaray.Pocketlink.svg
    install -Dm644 data/io.github.namaray.Pocketlink.Daemon.desktop {{autostart}}/io.github.namaray.Pocketlink.Daemon.desktop
    mkdir -p {{datadir}}/dbus-1/services
    sed 's|@BINDIR@|{{bindir}}|' data/io.github.namaray.Pocketlink.service.in > {{datadir}}/dbus-1/services/io.github.namaray.Pocketlink.service

# Remove everything `install` added. Keeps your pairings in ~/.config/pocketlink.
uninstall:
    rm -f {{bindir}}/pocketlink {{bindir}}/pocketlink-daemon
    rm -f {{datadir}}/applications/io.github.namaray.Pocketlink.desktop
    rm -f {{datadir}}/icons/hicolor/scalable/apps/io.github.namaray.Pocketlink.svg
    rm -f {{autostart}}/io.github.namaray.Pocketlink.Daemon.desktop
    rm -f {{datadir}}/dbus-1/services/io.github.namaray.Pocketlink.service
