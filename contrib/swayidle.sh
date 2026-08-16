#!/bin/sh
# wayqlo swayidle snippet
#
# swayidle has no config file, it is driven by command-line arguments.
# Add a line like this to your sway config (or run it directly):
#
#   exec swayidle -w \
#       timeout 300 'wayqlo' \
#       resume 'pkill wayqlo'
#
# Adjust the timeout (seconds) to taste. wayqlo exits on its own on any
# keypress or mouse movement, but the resume command is included as a
# safety net in case it is still running when the session resumes some
# other way.

swayidle -w \
    timeout 300 'wayqlo' \
    resume 'pkill wayqlo'
