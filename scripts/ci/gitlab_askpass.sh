#!/bin/sh
# The token stays out of Git remote URLs, command arguments, and logs.
case "$1" in
  *"Password for 'https://gitlab-ci-token@${CI_SERVER_HOST}':"*)
    printf '%s\n' "$CI_JOB_TOKEN" ;;

  *) exit 1 ;;
esac
