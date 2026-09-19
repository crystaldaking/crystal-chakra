# syntax=docker/dockerfile:1
#
# Containerized language-server environment for Chakra real-provider tests
# (issue #123). The image carries every supported language server pinned to
# the versions recorded in docs/languages/*.md, so real-provider tests
# (`-- --ignored` / `-- --include-ignored`) run without installing servers on
# the host. Use tools/run_lsp_tests.sh to build and run.
#
# Layers are ordered by change frequency: heavy, rarely-changing toolchains
# come first so rebuilds stay cheap.

FROM rust:1.97.1-bookworm

# rust-toolchain.toml pins channel 1.97.1 with rustfmt/clippy; rust-analyzer is
# the pinned rustup component required by the chakra-provider-rust-analyzer
# real-provider test (docs/languages/rust.md).
RUN rustup component add rustfmt clippy rust-analyzer

# Base utilities. python3 backs tools/*.py (probe_language_server.py,
# evaluate_php_lsp.py); xz-utils/unzip unpack server distributions.
RUN apt-get update \
    && apt-get install -y --no-install-recommends xz-utils unzip python3 \
    && rm -rf /var/lib/apt/lists/*

# Git inside the container runs as root against the host-mounted worktree, so
# the ownership check must not reject it.
RUN git config --system --add safe.directory /workspace

# clangd 21+ (docs/languages/cpp.md): pinned LLVM 21.1.8 binary distribution.
# Only the statically linked clangd binary and the clang builtin headers are
# kept; the full distribution is ~11 GB. The ~2 GB archive occasionally fails
# its checksum in transit (issue #183), so the download+verify pair retries
# with the failed archive deleted between attempts; the pinned digest itself
# is verified against upstream release metadata and is never weakened.
ARG LLVM_VERSION=21.1.8
ARG LLVM_SHA256=b3b7f2801d15d50736acea3c73982994d025b01c2f035b91ae3b49d1b575732b
RUN set -e; \
    for attempt in 1 2 3; do \
        rm -f /tmp/llvm.tar.xz; \
        if curl -fsSL "https://github.com/llvm/llvm-project/releases/download/llvmorg-${LLVM_VERSION}/LLVM-${LLVM_VERSION}-Linux-X64.tar.xz" \
                -o /tmp/llvm.tar.xz \
            && echo "${LLVM_SHA256}  /tmp/llvm.tar.xz" | sha256sum -c -; then \
            break; \
        fi; \
        if [ "${attempt}" -eq 3 ]; then \
            echo "LLVM archive failed its pinned checksum after ${attempt} attempts" >&2; \
            exit 1; \
        fi; \
        echo "LLVM archive download/checksum failed on attempt ${attempt}; retrying" >&2; \
    done; \
    mkdir -p /tmp/llvm \
    && tar -xJf /tmp/llvm.tar.xz -C /tmp/llvm --strip-components=1 \
    && mkdir -p /opt/llvm/bin /opt/llvm/lib \
    && cp /tmp/llvm/bin/clangd /opt/llvm/bin/clangd \
    && cp -r /tmp/llvm/lib/clang /opt/llvm/lib/clang \
    && rm -rf /tmp/llvm /tmp/llvm.tar.xz \
    && ln -s /opt/llvm/bin/clangd /usr/local/bin/clangd \
    && clangd --version

# JDK 21+ (docs/languages/java.md): pinned Temurin 21.0.6+7.
ARG JDK_VERSION=21.0.6_7
ARG JDK_SHA256=a2650fba422283fbed20d936ce5d2a52906a5414ec17b2f7676dddb87201dbae
RUN curl -fsSL "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.6%2B7/OpenJDK21U-jdk_x64_linux_hotspot_${JDK_VERSION}.tar.gz" \
        -o /tmp/jdk.tar.gz \
    && echo "${JDK_SHA256}  /tmp/jdk.tar.gz" | sha256sum -c - \
    && mkdir -p /opt/jdk \
    && tar -xzf /tmp/jdk.tar.gz -C /opt/jdk --strip-components=1 \
    && rm /tmp/jdk.tar.gz \
    && /opt/jdk/bin/java -version
ENV JAVA_HOME=/opt/jdk
ENV PATH="${JAVA_HOME}/bin:${PATH}"

# jdtls (docs/languages/java.md): pinned 1.60.0 milestone distribution with a
# `jdtls` launcher shim on PATH. The shim only proxies arguments: Chakra owns
# the per-workspace -data directory (ADR-0036); a default is supplied when the
# caller passes none.
ARG JDTLS_VERSION=1.60.0
ARG JDTLS_BUILD=202606262232
ARG JDTLS_SHA256=e94c303d8198f977930803582738771fd18c52c5492878410bf222b1aa81ef1d
RUN curl -fsSL "https://download.eclipse.org/jdtls/milestones/${JDTLS_VERSION}/jdt-language-server-${JDTLS_VERSION}-${JDTLS_BUILD}.tar.gz" \
        -o /tmp/jdtls.tar.gz \
    && echo "${JDTLS_SHA256}  /tmp/jdtls.tar.gz" | sha256sum -c - \
    && mkdir -p /opt/jdtls \
    && tar -xzf /tmp/jdtls.tar.gz -C /opt/jdtls \
    && rm /tmp/jdtls.tar.gz
RUN <<EOF
cat > /usr/local/bin/jdtls <<'SHIM'
#!/bin/sh
# jdtls launcher shim: proxies arguments to the equinox launcher pinned under
# /opt/jdtls. Chakra passes its own per-workspace `-data`; a default under the
# OS temporary directory is used otherwise.
JDTLS_HOME=/opt/jdtls
launcher=$(ls "$JDTLS_HOME"/plugins/org.eclipse.equinox.launcher_*.jar | sort | tail -n 1)
case " $* " in
    *" -data "*) set -- -configuration "$JDTLS_HOME/config_linux" "$@" ;;
    *) set -- -configuration "$JDTLS_HOME/config_linux" -data "${JDTLS_DATA:-/tmp/jdtls-data}" "$@" ;;
esac
exec java -jar "$launcher" "$@"
SHIM
chmod +x /usr/local/bin/jdtls
EOF

# .NET SDK 10 + csharp-ls 0.26.0 (docs/languages/csharp.md). Roll-forward lets
# the tool run on the installed major runtime.
ARG DOTNET_SDK_VERSION=10.0.400
ARG DOTNET_SDK_SHA512=1033977dd837150e0814cf0c5d5b17ceb63925fda7ba2158b47258a4bd7c048cf82eac3bc1166f3146f53124a3f5fba09db1de1260d2ce96399860303b404b48
ARG CSHARP_LS_VERSION=0.26.0
ENV DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1 \
    DOTNET_ROOT=/usr/local/dotnet \
    DOTNET_ROLL_FORWARD=LatestMajor \
    PATH="/root/.dotnet/tools:/usr/local/dotnet:${PATH}"
RUN curl -fsSL --retry 5 --retry-delay 10 --retry-all-errors \
        "https://builds.dotnet.microsoft.com/dotnet/Sdk/${DOTNET_SDK_VERSION}/dotnet-sdk-${DOTNET_SDK_VERSION}-linux-x64.tar.gz" \
        -o /tmp/dotnet-sdk.tar.gz \
    && echo "${DOTNET_SDK_SHA512}  /tmp/dotnet-sdk.tar.gz" | sha512sum -c - \
    && mkdir -p /usr/local/dotnet \
    && tar -xzf /tmp/dotnet-sdk.tar.gz -C /usr/local/dotnet \
    && rm /tmp/dotnet-sdk.tar.gz \
    && dotnet tool install --global csharp-ls --version "${CSHARP_LS_VERSION}" \
    && dotnet --list-sdks

# Go toolchain + gopls 0.23.x (docs/languages/go.md) + terraform-ls 0.39.x
# (docs/languages/hcl.md), both pinned through the Go module proxy.
ARG GO_VERSION=1.27.0
ARG GO_SHA256=675c26c449cbb18fc24b74650de1eabbae6e16f64326fd85a283fb3b58280685
ARG GOPLS_VERSION=v0.23.0
ARG TERRAFORM_LS_VERSION=v0.39.0
RUN curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-amd64.tar.gz" -o /tmp/go.tar.gz \
    && echo "${GO_SHA256}  /tmp/go.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/go.tar.gz -C /usr/local \
    && rm /tmp/go.tar.gz \
    && GOBIN=/usr/local/bin GOPATH=/go /usr/local/go/bin/go install "golang.org/x/tools/gopls@${GOPLS_VERSION}" \
    && GOBIN=/usr/local/bin GOPATH=/go /usr/local/go/bin/go install "github.com/hashicorp/terraform-ls@${TERRAFORM_LS_VERSION}" \
    && rm -rf /go /root/.cache/go-build \
    && gopls version \
    && terraform-ls version

# gopls shells out to the `go` command when loading a workspace view, so the
# Go toolchain must stay on PATH at test time.
ENV PATH="/usr/local/go/bin:${PATH}"

# Node.js + npm-installed servers (docs/languages/{python,typescript,shell}.md):
# pyright 1.1.413+, @vtsls/language-server with a resolvable typescript tsdk
# (vtsls exits silently without one), bash-language-server 5.6.x. NODE_PATH
# makes the global typescript resolvable from vtsls.
ARG NODE_VERSION=22.23.2
ARG NODE_SHA256=d60acfe00a2932254bb0ad20e01b0d74397a0875595de719654b214f4b03f307
ARG PYRIGHT_VERSION=1.1.413
ARG VTSLS_VERSION=0.3.0
ARG TYPESCRIPT_VERSION=5.9.3
ARG BASH_LS_VERSION=5.6.0
ENV NODE_PATH=/usr/local/lib/node_modules
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" -o /tmp/node.tar.xz \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
    && rm /tmp/node.tar.xz \
    && npm install --global \
        "pyright@${PYRIGHT_VERSION}" \
        "@vtsls/language-server@${VTSLS_VERSION}" \
        "typescript@${TYPESCRIPT_VERSION}" \
        "bash-language-server@${BASH_LS_VERSION}" \
    && npm cache clean --force \
    && pyright --version \
    && vtsls --version \
    && bash-language-server --version

# kotlin-server (Kotlin LSP) 263.4702.0 (docs/languages/kotlin.md): the
# standalone distribution bundles its own runtime; the pinned
# linux-x64 archive is checksum-verified and exposed as `kotlin-lsp` with
# `bin/intellij-server` as the launch target (ADR-0056).
ARG KOTLIN_LSP_VERSION=263.4702.0
ARG KOTLIN_LSP_SHA256=1e11d2e5fefbf9ea215ad8dd6be95f2222897cd086e8cb7a661a52084a590405
RUN curl -fsSL "https://download-cdn.jetbrains.com/language-server/kotlin-server/${KOTLIN_LSP_VERSION}/kotlin-server-${KOTLIN_LSP_VERSION}.tar.gz" -o /tmp/kotlin-server.tar.gz \
    && echo "${KOTLIN_LSP_SHA256}  /tmp/kotlin-server.tar.gz" | sha256sum -c - \
    && mkdir -p /opt/kotlin-lsp \
    && tar -xzf /tmp/kotlin-server.tar.gz -C /opt/kotlin-lsp --strip-components=1 \
    && rm /tmp/kotlin-server.tar.gz \
    && test -x /opt/kotlin-lsp/bin/intellij-server \
    && printf '#!/bin/sh\nexec /opt/kotlin-lsp/bin/intellij-server "$@"\n' > /usr/local/bin/kotlin-lsp \
    && chmod 0755 /usr/local/bin/kotlin-lsp \
    && test -x /usr/local/bin/kotlin-lsp

# The Kotlin real-provider fixture is a Maven project. Unlike jdtls, the
# standalone Kotlin importer launches an external mvn executable.
ARG MAVEN_VERSION=3.9.16
ARG MAVEN_SHA512=831a8591fe20c8243b1dbe7d71e3244f31d1665b0804b2e825e38cbbe5ce0cafb8338851f90780735568773e0a6cd07bbec107cda0b896b008b861075358b6f6
RUN (curl -fsSL --connect-timeout 20 --max-time 300 "https://dlcdn.apache.org/maven/maven-3/${MAVEN_VERSION}/binaries/apache-maven-${MAVEN_VERSION}-bin.tar.gz" -o /tmp/maven.tar.gz \
        || curl -fsSL --connect-timeout 20 --max-time 600 "https://archive.apache.org/dist/maven/maven-3/${MAVEN_VERSION}/binaries/apache-maven-${MAVEN_VERSION}-bin.tar.gz" -o /tmp/maven.tar.gz) \
    && echo "${MAVEN_SHA512}  /tmp/maven.tar.gz" | sha512sum -c - \
    && mkdir -p /opt/maven \
    && tar -xzf /tmp/maven.tar.gz -C /opt/maven --strip-components=1 \
    && rm /tmp/maven.tar.gz
ENV PATH="/opt/maven/bin:${PATH}"

# Android Kotlin import fixture: pinned SDK platform and build tools, fetched
# from Google's SDK repository. No emulator is needed for code intelligence.
ARG ANDROID_PLATFORM_SHA256=0988cacad01b38a18a47bac14a0695f246bc76c1b06c0eeb8eb0dc825ab0c8e0
ARG ANDROID_BUILD_TOOLS_SHA256=bd3a4966912eb8b30ed0d00b0cda6b6543b949d5ffe00bea54c04c81e1561d88
RUN curl -fsSL "https://dl.google.com/android/repository/platform-35_r02.zip" -o /tmp/android-platform.zip \
    && echo "${ANDROID_PLATFORM_SHA256}  /tmp/android-platform.zip" | sha256sum -c - \
    && curl -fsSL "https://dl.google.com/android/repository/build-tools_r35_linux.zip" -o /tmp/android-build-tools.zip \
    && echo "${ANDROID_BUILD_TOOLS_SHA256}  /tmp/android-build-tools.zip" | sha256sum -c - \
    && mkdir -p /opt/android-sdk/platforms /opt/android-sdk/build-tools \
    && unzip -q /tmp/android-platform.zip -d /opt/android-sdk/platforms \
    && unzip -q /tmp/android-build-tools.zip -d /opt/android-sdk/build-tools \
    && mv /opt/android-sdk/build-tools/android-15 /opt/android-sdk/build-tools/35.0.0 \
    && rm /tmp/android-platform.zip /tmp/android-build-tools.zip
ENV ANDROID_HOME=/opt/android-sdk

# KMP preparation invokes Gradle before LSP initialization. Production
# projects normally provide gradlew; the hermetic fixtures use this pinned
# executable so no developer-global Gradle installation is required.
ARG GRADLE_VERSION=8.14.3
ARG GRADLE_SHA256=bd71102213493060956ec229d946beee57158dbd89d0e62b91bca0fa2c5f3531
RUN curl -fsSL --connect-timeout 20 --max-time 600 "https://services.gradle.org/distributions/gradle-${GRADLE_VERSION}-bin.zip" -o /tmp/gradle.zip \
    && echo "${GRADLE_SHA256}  /tmp/gradle.zip" | sha256sum -c - \
    && unzip -q /tmp/gradle.zip -d /opt \
    && mv "/opt/gradle-${GRADLE_VERSION}" /opt/gradle \
    && rm /tmp/gradle.zip
ENV PATH="/opt/gradle/bin:${PATH}"

# Final resolvability check for providers and the Kotlin fixture's build tool.
RUN for exe in rust-analyzer clangd gopls pyright-langserver vtsls \
        bash-language-server jdtls csharp-ls terraform-ls kotlin-lsp mvn gradle; do \
        command -v "$exe" >/dev/null || { echo "missing: $exe"; exit 1; }; \
    done \
    && rust-analyzer --version \
    && python3 --version \
    && mvn --version

WORKDIR /workspace
CMD ["bash"]
