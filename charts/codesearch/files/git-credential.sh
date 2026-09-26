#!/bin/sh
# git credential helper: answers `get` with GIT_USERNAME / GIT_TOKEN from the environment.
[ "$1" = get ] || exit 0
echo "username=${GIT_USERNAME:-x-access-token}"
echo "password=${GIT_TOKEN}"
