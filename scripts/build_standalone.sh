#!/bin/bash
# Build profile_aggregator Docker image (standalone repository version)

set -e

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${YELLOW}Building profile_aggregator Docker image for linux/amd64...${NC}"

# Get the repository root (when profile_aggregator is its own repo)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Check if we're in the right directory
if [ ! -f "$REPO_ROOT/Cargo.toml" ] || [ ! -f "$REPO_ROOT/Dockerfile" ]; then
    echo -e "${RED}Error: Must be run from the profile_aggregator repository${NC}"
    exit 1
fi

echo -e "${YELLOW}Building from repository root: $REPO_ROOT${NC}"

# Build the image
cd "$REPO_ROOT"
docker buildx build --platform linux/amd64 \
  -f Dockerfile \
  -t ghcr.io/verse-pbc/profile_aggregator:latest \
  . \
  --load

echo -e "${GREEN}Build complete!${NC}"
echo -e "${GREEN}Image tagged as: ghcr.io/verse-pbc/profile_aggregator:latest${NC}"

# Show image info
docker images ghcr.io/verse-pbc/profile_aggregator:latest