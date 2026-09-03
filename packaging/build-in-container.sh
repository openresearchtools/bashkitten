#!/bin/sh
set -eu

cd /source
cargo test --locked
cargo build --locked --release

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
package_root=/build/package-root
artifact=/build/artifacts/bashkitten_${version}_amd64.deb

rm -rf "$package_root"
install -d \
    "$package_root/DEBIAN" \
    "$package_root/usr/bin" \
    "$package_root/usr/lib/systemd/user" \
    "$package_root/usr/share/applications" \
    "$package_root/usr/share/icons/hicolor/scalable/apps" \
    "$package_root/usr/share/doc/bashkitten"

for binary in bashkitten bashkitten-agent bashkitten-web bashkitten-controller; do
    install -m 0755 "/build/target/release/$binary" "$package_root/usr/bin/$binary"
done

install -m 0644 packaging/systemd/bashkitten.target "$package_root/usr/lib/systemd/user/bashkitten.target"
install -m 0644 packaging/systemd/bashkitten-web.service "$package_root/usr/lib/systemd/user/bashkitten-web.service"
install -m 0644 packaging/systemd/bashkitten-llama.service "$package_root/usr/lib/systemd/user/bashkitten-llama.service"
install -m 0644 packaging/systemd/bashkitten-controller.service "$package_root/usr/lib/systemd/user/bashkitten-controller.service"
install -m 0644 packaging/bashkitten.desktop "$package_root/usr/share/applications/bashkitten.desktop"
install -m 0644 packaging/bashkitten.svg "$package_root/usr/share/icons/hicolor/scalable/apps/bashkitten.svg"
install -m 0644 LICENSE "$package_root/usr/share/doc/bashkitten/copyright"
install -m 0644 PI_UPSTREAM.md "$package_root/usr/share/doc/bashkitten/PI_UPSTREAM.md"
install -m 0644 README.md "$package_root/usr/share/doc/bashkitten/README.md"
install -m 0644 THIRD_PARTY_NOTICES.md "$package_root/usr/share/doc/bashkitten/THIRD_PARTY_NOTICES.md"

installed_size=$(du -sk "$package_root/usr" | awk '{print $1}')
cat >"$package_root/DEBIAN/control" <<EOF
Package: bashkitten
Version: $version
Section: devel
Priority: optional
Architecture: amd64
Maintainer: OpenResearchTools <openresearchtools@users.noreply.github.com>
Homepage: https://github.com/openresearchtools/bashkitten
Depends: libc6, libgtk-4-1, systemd, ripgrep, fd-find
Installed-Size: $installed_size
Description: Minimal standalone Rust coding-agent Web UI
 BashKitten provides isolated agent-session processes, a local authenticated
 Web interface, and a small GTK lifecycle controller without Node.js or npm.
EOF

dpkg-deb --root-owner-group --build "$package_root" "$artifact"
dpkg-deb --info "$artifact"
(cd "$(dirname "$artifact")" && sha256sum "$(basename "$artifact")" >"$(basename "$artifact").sha256")
printf '%s\n' "$artifact"
