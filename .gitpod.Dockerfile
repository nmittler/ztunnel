# Remote docker environment for ztunnel development with Gitpod (https://www.gitpod.io/).
# Based on https://github.com/JetBrains/clion-remote/blob/master/Dockerfile.remote-cpp-env
#
# Build:
#   docker build -t nmittler/ztunnel-gitpod:0.1 -f .gitpod.Dockerfile .
#
# To force Gitpod workspace to rebuild, open the following URL in a browser:
#   https://gitpod.io/#imagebuild/https://github.com/<istio|username>/ztunnel[/tree/<branch>]
#
#   e.g. https://gitpod.io/#imagebuild/https://github.com/nmittler/ztunnel/gitpod
#
# Additional information:
#   https://www.gitpod.io/docs/configure/workspaces/workspace-image#manually-rebuild-a-workspace-image

FROM gcr.io/istio-testing/build-tools:master-65b95c3425a26e633081b2d0834cc0df6e81fd8a

# Install:
# - git (and git-lfs), for git operations (to e.g. push your work).
#   Also required for setting up your configured dotfiles in the workspace.
# - sudo, while not required, is recommended to be installed, since the
#   workspace user (`gitpod`) is non-root and won't be able to install
#   and use `sudo` to install any other tools in a live workspace.
RUN apt-get update && apt-get install -yq \
    git \
    git-lfs \
    sudo \
    && apt-get clean && rm -rf /var/lib/apt/lists/* /tmp/*

# Create the gitpod user. UID must be 33333.
RUN useradd -l -u 33333 -G sudo -md /home/gitpod -s /bin/bash -p gitpod gitpod

USER gitpod
RUN cp -r /home/.rustup /home/gitpod/

#FROM gitpod/workspace-rust
#
#RUN sudo apt-get update \
#  && sudo apt-get install -y --no-install-recommends \
#      apt-transport-https \
#      build-essential \
#      ca-certificates \
#      iptables \
#      iproute2 \
#      wget \
#      curl \
#      gnupg2 \
#      software-properties-common \
#      unzip \
#      xz-utils \
#      gcc \
#      g++ \
#      gdb \
#      clang \
#      make \
#      ninja-build \
#      cmake \
#      autoconf \
#      automake \
#      libtool \
#      valgrind \
#      locales-all \
#      dos2unix \
#      rsync \
#      tar \
#      protobuf-compiler \
#  && sudo apt-get clean
#
##ENV RUSTUP_HOME=${HOME}/.rustup
#ENV CARGO_HOME=/workspace/.cargo
#ENV PATH=${CARGO_HOME}/bin:$PATH
#
## The Gitpod image (https://github.com/gitpod-io/workspace-images/blob/main/chunks/lang-rust/Dockerfile)
## sets the default toolchain to stable. We need to switch to nightly for features.
##RUN rustup set profile default
#RUN rustup default nightly
#
#RUN rustup --version; \
#    cargo --version; \
#    rustc --version;
