#!/usr/bin/env bash
set -euo pipefail

prefix=""
with_helper=false

while [ "$#" -gt 0 ]; do
  case "$1" in
    --prefix)
      [ "$#" -ge 2 ] || { printf '%s\n' 'error: --prefix requires a path' >&2; exit 2; }
      case "$2" in
        -*) printf '%s\n' 'error: --prefix requires a path, not another option' >&2; exit 2 ;;
      esac
      prefix="$2"
      shift 2
      ;;
    --with-helper)
      with_helper=true
      shift
      ;;
    -h|--help)
      printf '%s\n' 'Usage: scripts/install-local.sh [--prefix PATH] [--with-helper]'
      exit 0
      ;;
    *)
      if [ -z "$prefix" ]; then
        prefix="$1"
        shift
      else
        printf 'error: unexpected argument: %s\n' "$1" >&2
        exit 2
      fi
      ;;
  esac
done

prefix="${prefix:-$HOME/.local}"
case "$prefix" in
  /*) ;;
  *) printf '%s\n' 'error: install prefix must be an absolute path' >&2; exit 2 ;;
esac
if $with_helper && [ "$(id -u)" -ne 0 ]; then
  printf '%s\n' 'error: --with-helper installs a root-owned polkit boundary and must run as root' >&2
  exit 1
fi

mkdir -p "$prefix/bin"
cargo build --release --locked -p reginux-tui --bin reginux -p reginux-core --bin reginux-sandbox
install -m 0755 target/release/reginux "$prefix/bin/reginux"
install -m 0755 target/release/reginux-sandbox "$prefix/bin/reginux-sandbox"

if $with_helper; then
  cargo build --release --locked -p reginux-tui --bin reginux-helper
  install -d -m 0755 /usr/libexec /usr/share/polkit-1/actions
  install -o root -g root -m 0755 target/release/reginux-helper /usr/libexec/reginux-helper
  install -o root -g root -m 0644 resources/polkit/org.reginux.apply.policy /usr/share/polkit-1/actions/org.reginux.apply.policy
  printf 'Installed Reginux to %s/bin and the polkit helper boundary to /usr/libexec\n' "$prefix"
else
  printf 'Installed Reginux and its mandatory command sandbox to %s/bin\n' "$prefix"
fi
