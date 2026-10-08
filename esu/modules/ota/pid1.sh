#!/bin/sh
# Publish one switch device per declared base image of the selected ROM.
# Runs after the `thin` entry activated every staging LV and before `gpt`
# resolves the projection; a ROM 1 exits 0 without touching a device.
set -eu
exec ota-stage
