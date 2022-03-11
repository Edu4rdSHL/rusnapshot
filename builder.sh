#!/usr/bin/env bash
NAME="rusnapshot"

LINUX_TARGET="x86_64-unknown-linux-musl"
LINUX_X86_TARGET="i686-unknown-linux-musl"
MANPAGE_DIR="./$NAME.1"

if ! systemctl is-active docker >/dev/null 2>&1; then
  echo "Docker is not running. Starting docker."
  if ! sudo systemctl start docker; then
    echo "Failed to start docker."
    exit 1
  fi
fi

# Linux build
echo "Building Linux artifact."
if cargo build -q --release --target="$LINUX_TARGET"; then
  echo "Linux artifact build: SUCCESS"
  cp "target/$LINUX_TARGET/release/$NAME" "target/$LINUX_TARGET/release/$NAME-linux"
  strip "target/$LINUX_TARGET/release/$NAME-linux"
  sha512sum "target/$LINUX_TARGET/release/$NAME-linux" >"target/$LINUX_TARGET/release/$NAME-linux.sha512"
else
  echo "Linux artifact build: FAILED"
fi

# Linux x86 build
echo "Building Linux x86 artifact."
if cross build -q --release --target="$LINUX_X86_TARGET"; then
  echo "Linux x86 artifact build: SUCCESS"
  cp "target/$LINUX_X86_TARGET/release/$NAME" "target/$LINUX_X86_TARGET/release/$NAME-linux-i386"
  strip "target/$LINUX_X86_TARGET/release/$NAME-linux-i386"
  sha512sum "target/$LINUX_X86_TARGET/release/$NAME-linux-i386" >"target/$LINUX_X86_TARGET/release/$NAME-linux-i386.sha512"
else
  echo "Linux x86 artifact build: FAILED"
fi

echo "Creating manpage..."
if command -v help2man >/dev/null; then
  if help2man -o "$MANPAGE_DIR" "target/$LINUX_TARGET/release/$NAME"; then
    echo "Manpage created sucessfully and saved in $MANPAGE_DIR"
  else
    echo "Error creating manpage."
  fi
else
  echo "Please install the help2man package."
fi

# Stop docker
echo "Stopping docker."
if ! sudo systemctl stop docker; then
  echo "Failed to stop docker."
  exit 1
fi
