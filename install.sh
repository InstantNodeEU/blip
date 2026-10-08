#!/bin/sh
# Installs blip from the latest release:
#
#   curl -fsSL https://raw.githubusercontent.com/instantnodeeu/blip/main/install.sh | sudo sh
#
# As root on a systemd box it also sets up /etc/blip/blip.conf and the blip
# service (an existing config is left alone). Without root the binary goes to
# ~/.local/bin and nothing else is touched. BINDIR picks another directory,
# VERSION=v0.1.0 a specific release. Run it again to update, a running service
# is restarted.
set -eu

REPO=instantnodeeu/blip
NAME=blip

die() { printf 'install: %s\n' "$*" >&2; exit 1; }

# fetch <url> <file>
fetch() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --retry 3 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO "$2" "$1"
	else
		die "needs curl or wget"
	fi
}

[ "$(uname -s)" = Linux ] || die "release builds are for linux only, build it with cargo on $(uname -s)"
case $(uname -m) in
	x86_64 | amd64) arch=x86_64 ;;
	aarch64 | arm64) arch=aarch64 ;;
	*) die "no release build for $(uname -m), build it with cargo" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

version=${VERSION:-}
if [ -z "$version" ]; then
	fetch "https://api.github.com/repos/$REPO/releases/latest" "$tmp/latest"
	version=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp/latest")
	[ -n "$version" ] || die "could not find the latest release, set VERSION=v0.1.0"
fi

asset=$NAME-$version-$arch-unknown-linux-musl
url=https://github.com/$REPO/releases/download/$version
fetch "$url/$asset" "$tmp/$asset" || die "download failed: $url/$asset"
fetch "$url/$asset.sha256" "$tmp/sum" || die "download failed: $url/$asset.sha256"
want=$(awk '{ print $1; exit }' "$tmp/sum")
if command -v sha256sum >/dev/null 2>&1; then sum=sha256sum
elif command -v shasum >/dev/null 2>&1; then sum="shasum -a 256"
else die "needs sha256sum or shasum to check the download"; fi
got=$($sum "$tmp/$asset" | awk '{ print $1 }')
[ -n "$want" ] && [ "$got" = "$want" ] || die "checksum mismatch for $asset, not installing it"

if [ -z "${BINDIR:-}" ]; then
	if [ "$(id -u)" = 0 ] || [ -w /usr/local/bin ]; then BINDIR=/usr/local/bin; else BINDIR=$HOME/.local/bin; fi
fi
mkdir -p "$BINDIR"
# rename over the old binary so a running service isn't touched
cp "$tmp/$asset" "$BINDIR/.$NAME.new"
chmod 755 "$BINDIR/.$NAME.new"
mv -f "$BINDIR/.$NAME.new" "$BINDIR/$NAME"
echo "installed $NAME $version to $BINDIR/$NAME"

# the unit expects /usr/local/bin/blip and /etc/blip/blip.conf
if [ "$(id -u)" = 0 ] && [ -d /run/systemd/system ] && [ "$BINDIR" = /usr/local/bin ]; then
	raw=https://raw.githubusercontent.com/$REPO/$version
	if [ ! -f /etc/blip/blip.conf ]; then
		mkdir -p /etc/blip
		fetch "$raw/blip.conf.example" /etc/blip/blip.conf
		echo "wrote /etc/blip/blip.conf, add your checks there"
	fi
	if [ ! -f /etc/systemd/system/blip.service ]; then
		fetch "$raw/contrib/blip.service" /etc/systemd/system/blip.service
		systemctl daemon-reload
	fi
	if systemctl is-active --quiet blip; then
		systemctl restart blip
		echo "restarted the blip service"
	else
		echo "start it with: systemctl enable --now blip"
	fi
	echo "run this again to update. To remove: systemctl disable --now blip && rm $BINDIR/$NAME /etc/systemd/system/blip.service"
	exit 0
fi

case :$PATH: in
	*:"$BINDIR":*) ;;
	*) echo "$BINDIR is not in your PATH, add it or run $BINDIR/$NAME" ;;
esac
echo "run this again to update, rm $BINDIR/$NAME to remove"
