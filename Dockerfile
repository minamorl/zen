# Using Node.js 26.12.1
# Official Node distribution: no dependency on an unpublished multiarch image.
FROM debian:bookworm-slim AS node26-runtime
ARG NODE_VERSION=26.12.1
ARG TARGETARCH
ENV NODE_VERSION=${NODE_VERSION}
RUN set -eux; \
    test "$NODE_VERSION" = 26.12.1; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl xz-utils libstdc++6 libatomic1; \
    case "$TARGETARCH" in \
      arm64) arch=arm64; checksum=42aa04922591ee5d4dbc7740d452621acf28508239daa512f5ee8c47ee224fdb ;; \
      amd64) arch=x64; checksum=6170b2d474e1c626f43cff3adc118df9fe3b573258a4fc3d850c8dbc6b079bbd ;; \
      *) echo "unsupported Node architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    archive="node-v${NODE_VERSION}-linux-${arch}.tar.xz"; \
    curl --fail --location --proto '=https' --tlsv1.2 -o /tmp/node.tar.xz "https://nodejs.org/dist/v${NODE_VERSION}/${archive}"; \
    echo "$checksum  /tmp/node.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1; \
    rm /tmp/node.tar.xz; rm -rf /var/lib/apt/lists/*; \
    groupadd --gid 1000 node; useradd --uid 1000 --gid node --create-home node; \
    test "$(node --version)" = "v${NODE_VERSION}"; npm --version

FROM node26-runtime

# Setting the working directory
WORKDIR /app

RUN npm install -g pnpm
# Copying package.json and package-lock.json
#COPY package.json package-lock.json ./
#COPY prisma ./prisma/

# Installing dependencies
#RUN npm install

# Copying the rest of the application's code
COPY . .

# Exposing port 3000
EXPOSE 3000

# Starting the development server
CMD ["pnpm", "run", "dev"]
