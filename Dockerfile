# Builds SevenMCP-x86.exe and SevenMCP-x64.exe:  docker build -o out .
FROM --platform=linux/amd64 fedora:44 AS toolchain
RUN dnf install -y clang llvm bsdtar && dnf clean all

ARG RUST9X=rust9x-1.99.1-v1
RUN curl -fsSL https://github.com/rust9x/rust/releases/download/$RUST9X/$RUST9X-x86_64-unknown-linux-gnu.tar.xz \
    | bsdtar xf - -C /opt && mv /opt/$RUST9X-* /opt/rust9x
ENV PATH=/opt/rust9x/bin:$PATH

# MSVC CRT & Windows SDK import libraries from Microsoft.
# building accepts their license.
ARG XWIN=0.10.0
RUN curl -fsSL https://github.com/Jake-Shadle/xwin/releases/download/$XWIN/xwin-$XWIN-x86_64-unknown-linux-musl.tar.gz \
    | bsdtar xf - -C /usr/local/bin --strip-components=1 xwin-$XWIN-x86_64-unknown-linux-musl/xwin \
    && xwin --accept-license --arch x86,x86_64 splat --output /xwin && rm -rf .xwin-cache
ENV CC=clang-cl AR=llvm-lib \
    INCLUDE=/xwin/crt/include;/xwin/sdk/include/ucrt;/xwin/sdk/include/um;/xwin/sdk/include/shared

FROM toolchain AS build
WORKDIR /src
COPY . .
# a commit makes a dev build:  docker build --build-arg SEVENMCP_DEV=$(git rev-parse HEAD) -o out .
ARG SEVENMCP_DEV
RUN --mount=type=cache,target=/root/.cargo/registry --mount=type=cache,target=/src/target \
    for t in x86:i686 x64:x86_64; do \
        arch=${t%%:*} triple=${t#*:}-rust9x-windows-msvc; xarch=$([ $arch = x64 ] && echo x86_64 || echo x86); \
        LIB="/xwin/crt/lib/$xarch;/xwin/sdk/lib/um/$xarch;/xwin/sdk/lib/ucrt/$xarch" \
        cargo build --release --locked --target $triple \
        && cp target/$triple/release/sevenmcp.exe /SevenMCP-$arch.exe || exit 1; \
    done

FROM scratch
COPY --from=build /SevenMCP-*.exe /
