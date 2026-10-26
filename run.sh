#!/usr/bin/env bash
cd "$(dirname "$0")"
exec /run/current-system/sw/bin/python3 main.py "$@"
