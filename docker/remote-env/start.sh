#!/bin/bash

# Run the docker container, mapping the source code from this host.
# Privileged mode is required for allowing the container to create
# and manage tun devices.
docker run -d \
  --privileged \
  -p 127.0.0.1:2222:22 \
  --name ztunnel-dev \
  --mount type=bind,source="$PWD",target="/home/user/ztunnel" \
  ztunnel/remote-env:0.1
