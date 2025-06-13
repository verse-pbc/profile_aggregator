#!/bin/bash
# Build profile_aggregator Docker image for linux/amd64

set -e

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${YELLOW}Building profile_aggregator Docker image for linux/amd64...${NC}"

# Get the script directory and workspace root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(dirname "$SCRIPT_DIR")"
WORKSPACE_ROOT="$(cd "$CRATE_DIR/../.." && pwd)"

# Check if we can find the workspace root
if [ ! -f "$WORKSPACE_ROOT/Cargo.toml" ]; then
    echo -e "${RED}Error: Cannot find workspace root directory${NC}"
    echo -e "${YELLOW}Expected to find Cargo.toml at: $WORKSPACE_ROOT${NC}"
    exit 1
fi

echo -e "${YELLOW}Building from workspace root: $WORKSPACE_ROOT${NC}"

# Build the image from workspace root
cd "$WORKSPACE_ROOT"
docker buildx build --platform linux/amd64 \
  -f crates/profile_aggregator/Dockerfile \
  -t ghcr.io/verse-pbc/profile_aggregator:latest \
  . \
  --load

echo -e "${GREEN}Build complete!${NC}"
echo -e "${GREEN}Image tagged as: ghcr.io/verse-pbc/profile_aggregator:latest${NC}"

# Show image info
docker images ghcr.io/verse-pbc/profile_aggregator:latest