#!/bin/sh
set -eu

# Secret validation and loading belong to bot-config so plaintext credentials
# never become process environment variables. Compose/Kubernetes supplies only
# *_FILE paths; xiao-bot opens each owner-only file with no-follow semantics.
# This entrypoint deliberately does no argument- or mode-specific setup: the
# binary selects `migrate` before loading the full runtime configuration.
exec /usr/local/bin/xiao-bot "$@"
