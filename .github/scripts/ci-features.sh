#!/usr/bin/env bash
# The cargo feature selection CI builds, lints and tests the workspace with on this host.
#
# Everywhere but Linux that is every feature of every member. On Linux, `tls-native-tls` is OpenSSL,
# which cannot share a binary with `http3` (quiche's BoringSSL exports the same `SSL_*` symbols), so
# the selection is every feature but the ones that bring OpenSSL in.
#
# Without an argument it prints the selection for one cargo invocation over the workspace
# (`member/feature` names). With `--per-package` it prints it for cargo-hack, which checks each
# member on its own manifest: the members' feature names without `tls-native-tls`, each member
# taking the ones it has.
set -euo pipefail

mode="${1:-workspace}"

case "$(uname -s)" in
  Linux)
    case "$mode" in
      workspace)
        cargo metadata --no-deps --format-version 1 | jq -r '
          ["skyzen/tls-native-tls", "skyzen-services/tls-native-tls"] as $openssl
          | [.packages[] as $package | $package.features | keys[] | "\($package.name)/\(.)"]
          | map(select(. as $feature | $openssl | index($feature) | not))
          | "--features=" + join(",")'
        ;;
      --per-package)
        cargo metadata --no-deps --format-version 1 | jq -r '
          [.packages[].features | keys[]] | unique
          | map(select(. != "tls-native-tls"))
          | "--ignore-unknown-features --features=" + join(",")'
        ;;
      *)
        echo "unknown mode: $mode" >&2
        exit 64
        ;;
    esac
    ;;
  *)
    echo "--all-features"
    ;;
esac
