# Baseline comparison image: VS Code + java-vsix-lite + redhat.java, all pinned.
# Build context is the REPOSITORY ROOT (see run.sh).
FROM eclipse-temurin:21-jdk-jammy

ARG NODE_VERSION=20.20.2
ARG NODE_SHA256_AMD64=df770b2a6f130ed8627c9782c988fda9669fa23898329a61a871e32f965e007d
ARG NODE_SHA256_ARM64=73093db209e4e9e09dd7d15a47aeaab1b74833830df03efa5f942a1122c5fa71
ARG RUST_VERSION=1.98.1
ARG VSCODE_VERSION=1.137.0
ARG REDHAT_JAVA_URL=https://open-vsx.org/api/redhat/java/1.57.2026090408/file/redhat.java-1.57.2026090408.vsix
ARG REDHAT_JAVA_SHA256=2249b3669443f88e82243ad4fae8b1247741bafe341f52d1e6d15cd2e720ed6e

ENV DEBIAN_FRONTEND=noninteractive

# VS Code's Electron runtime needs a GUI stack even headless; xvfb provides
# the display. `build-essential` provides the C compiler and linker cargo
# needs both to link `jvl-server` and to compile tree-sitter-java's bundled
# C parser (`cc` crate build script). The rest are Chromium's shared-library
# dependencies.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl xz-utils unzip git xvfb xauth build-essential \
      libnss3 libatk1.0-0 libatk-bridge2.0-0 libcups2 libdrm2 libgbm1 \
      libgtk-3-0 libasound2 libxkbcommon0 libxcomposite1 libxdamage1 \
      libxfixes3 libxrandr2 libpango-1.0-0 libcairo2 libx11-xcb1 \
      libsecret-1-0 fonts-liberation \
    && rm -rf /var/lib/apt/lists/*

# Node (pinned tarball + checksum, arch-detected). VS Code's Electron runtime
# under QEMU user-mode emulation is not merely slow but memory-prohibitive in
# practice (a bare `code --install-extension` alone was observed to grow past
# 7.6 GB RSS and get OOM-killed) — so the image builds and runs natively on
# whichever architecture it's built on, amd64 or arm64, rather than always
# targeting amd64.
RUN set -eu; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      amd64) node_arch=x64; node_sha256="${NODE_SHA256_AMD64}" ;; \
      arm64) node_arch=arm64; node_sha256="${NODE_SHA256_ARM64}" ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/node.tar.xz "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${node_arch}.tar.xz"; \
    echo "${node_sha256}  /tmp/node.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1; \
    rm /tmp/node.tar.xz; \
    node --version; \
    echo "$node_arch" > /opt/node-arch.txt

# Rust (pinned toolchain) for building jvl-server.
RUN curl -fsSL https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain "${RUST_VERSION}" \
    && rm /tmp/rustup.sh
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p jvl-server --locked

COPY editors/vscode ./editors/vscode
WORKDIR /work/editors/vscode
# `rm -f *.vsix` guards against stray VSIX files a host `npm run package:vsix`
# run may have left in this gitignored (but dockerignore-nested) directory;
# without it a stale file plus the freshly packaged one would make the final
# `cp` ambiguous.
RUN npm ci && npm run build \
    && rm -f *.vsix \
    && node scripts/package-vsix.mjs --no-build \
    && mkdir -p /opt/vsix \
    && cp java-vsix-lite-*.vsix /opt/vsix/java-vsix-lite.vsix

# redhat.java, pinned and checksum-verified.
RUN curl -fsSLo /opt/vsix/redhat-java.vsix "${REDHAT_JAVA_URL}" \
    && echo "${REDHAT_JAVA_SHA256}  /opt/vsix/redhat-java.vsix" | sha256sum -c -

# Prefetch the pinned VS Code build (matching the detected architecture) so
# `docker run` needs no network.
ENV VSCODE_VERSION=${VSCODE_VERSION}
ENV NODE_PATH=/work/editors/vscode/node_modules
RUN node -e "const {downloadAndUnzipVSCode}=require('@vscode/test-electron');\
const nodeArch=require('fs').readFileSync('/opt/node-arch.txt','utf8').trim();\
const platform=nodeArch==='arm64'?'linux-arm64':'linux-x64';\
downloadAndUnzipVSCode({version:process.env.VSCODE_VERSION,platform,cachePath:'/opt/vscode-test-cache'})\
.then(p=>require('fs').writeFileSync('/opt/vscode-exec-path.txt',p));" \
    && cat /opt/vscode-exec-path.txt

COPY tools/vscode-compare/harness /work/tools/vscode-compare/harness
COPY tools/vscode-compare/project /work/tools/vscode-compare/project
RUN chmod +x /work/tools/vscode-compare/harness/*.sh

# The fixture is a real Maven project, so both plugins resolve dependencies
# the same way: read-only, out of ~/.m2/repository. Populate that repository
# now, while the build still has network, so `docker run` needs none. The
# `install` (not just `dependency:go-offline`) also pulls the plugins jdt.ls
# asks for during a Maven import, and leaves a compiled target/ behind,
# which is exactly the state a developer's checkout would be in.
RUN apt-get update && apt-get install -y --no-install-recommends maven \
    && rm -rf /var/lib/apt/lists/* \
    && mvn -B -q -f /work/tools/vscode-compare/project/pom.xml \
         dependency:go-offline install -DskipTests \
    && mvn -B -q -f /work/tools/vscode-compare/project/pom.xml clean \
    && printf '%s\n' \
         '<settings xmlns="http://maven.apache.org/SETTINGS/1.0.0">' \
         '  <!-- The repository below is fully warmed at image build time;' \
         '       offline makes any stray resolution fail fast at run time' \
         '       instead of hanging on a network that is not there. -->' \
         '  <offline>true</offline>' \
         '</settings>' \
         > /root/.m2/settings.xml \
    && du -sh /root/.m2/repository

ENV JVL_COMPARE_PROJECT=/work/tools/vscode-compare/project
ENV JVL_COMPARE_OUT_DIR=/out

CMD ["/work/tools/vscode-compare/harness/entrypoint.sh"]
